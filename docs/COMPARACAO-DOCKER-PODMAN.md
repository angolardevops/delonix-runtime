# Análise de Gaps — delonix-runtime vs Docker/Podman rootless em produção

> Actualizado 2026-07-23. Revisão desde a versão anterior: pods reais multi-container
> (netns+IPC+UTS), `delonix cluster kubeadm` validado ponta-a-ponta com um control-plane
> k8s v1.34 `Ready` real, `vm bridge` (VM↔container por IP directo), diagnóstico de crash
> + re-supervisão de `--restart`, **a auditoria de segurança adversarial de 2026-07-21**
> (`docs/AUDITORIA-E2E.md`) e, no mesmo dia, **a correcção dos 6 HIGH que ela confirmou**
> (ver secção 1a — corrigidos, mas ainda por CONFIRMAR por uma 2.ª auditoria independente).

## 1. Veredicto executivo

O **delonix-runtime não é hoje um substituto drop-in do Docker/Podman rootless**, mas está muito mais perto do que o CLAUDE.md sugere — e em várias dimensões ultrapassa ambos. A distância não é uniforme: é moderada em ciclo de vida de containers, rede e volumes, e grande em build de imagens, compatibilidade de ecossistema (API/compose/tooling) e GPU. **Em segurança, o desenho é sólido (userns/seccomp/caps/fail-closed por omissão) e os 6 bugs HIGH que a execução tinha foram corrigidos em 2026-07-23** (ver secção 1a) — mas o núcleo de syscalls nunca teve revisão adversarial e os fixes ainda não foram confirmados por um 2.º par de olhos independente, por isso o cuidado antes de expor a estranhos mantém-se.

**Para que casos JÁ serve (com confiança):**
- **Execução e operação interactiva de containers** — run/ps/stop/exec/logs/inspect + extras que o Docker não tem (reconfiguração a quente, pause via freezer, describe estilo kubectl, diagnóstico automático de crash com razão+forense).
- **Distribuição de imagens OCI** — pull/push/tag/history/login interoperáveis com registos, com assinatura cosign e scan de CVE embutidos (diferenciais).
- **Rede de container single-node** — `--net host/none/bridge-custom`, publish rootless via slirp4netns, DNS de descoberta com isolamento por namespace, overlay VXLAN+WireGuard, firewall L4/egress e shaping — supera o podman rootless em várias frentes.
- **Pods reais multi-container** (`kind: Pod`) — netns + IPC + UTS partilhados, validado E2E.
- **Bootstrap de Kubernetes SEM Docker** — servidor CRI real para kubelet, imagem VM dourada, `cluster kubeadm`, e **modo Kind (`kindest/node`) já ARRANCA e um control-plane v1.34 fica `Ready`** (netfilter/cgroup2/containerd todos resolvidos). Terreno onde é motor único (container + VM + k8s), ninguém no espaço Docker/Podman cobre este arco.

**Para que NÃO serve (ainda), por duas razões distintas:**
- **Segurança ainda não confirmada de forma independente** — os 6 HIGH conhecidos estão corrigidos (secção 1a), mas o núcleo de syscalls (104 `unsafe`) nunca foi auditado e ninguém verificou os fixes de fora para dentro. Para um host multi-utilizador ou que corra imagens/manifestos de terceiros não confiáveis, prudência até uma 2.ª auditoria confirmar.
- **Compatibilidade de ecossistema (limita o âmbito, não a segurança)** — só single-stage build, sem BuildKit/buildx, sem API Docker-compatível (logo sem `docker compose`/testcontainers/CI via `DOCKER_HOST`), sem GPU/CDI real. Não impede um beta honesto sobre o que cobre.

**Posição global:** um runtime rootless-first **sólido em desenho e em pontos superior** para operação directa e para o caminho Kubernetes, com os bugs de segurança conhecidos já corrigidos, mas **ainda sem confirmação adversarial independente** e **não interoperável com o ecossistema Docker**. Não é ainda "drop-in" — a barreira nº1 deixou de ser os 6 HIGH (fechados) e passa a ser: confirmar essa correcção de fora para dentro, cobrir o núcleo de syscalls que nunca foi revisto, e só depois falar de compatibilidade de superfície.

---

## 1a. Segurança — 6 HIGH da auditoria de 2026-07-21, CORRIGIDOS em 2026-07-23

Fonte completa: [`docs/AUDITORIA-E2E.md`](AUDITORIA-E2E.md) (24 achados confirmados por 2 céticos adversariais + 11 candidatos ainda por verificar).

| # | Achado | Impacto | Local | Estado |
|---|---|---|---|---|
| 1 | Path traversal em whiteouts OCI | Imagem maliciosa apaga ficheiros/directórios arbitrários do utilizador (ex.: a home inteira) — reachable no `container run` rootless DEFAULT | `delonix-image/src/overlay.rs` | ✅ Corrigido — `safe_rel` no ramo de whiteout + confinamento contra symlink plantado |
| 2 | IDs do CRI sem validação | Kubelet comprometido apaga/lê `*.json` arbitrário via `../` | `delonix-cri/src/runtime_svc/lifecycle.rs` | ✅ Corrigido — whitelist centralizada em `write_rec`/`read_rec`/`remove_rec` |
| 3 | Nome de VM ainda escapa o fix anterior | `generate_seed_iso` escrevia ficheiros fora do state-dir ANTES de `create()` validar o nome | `cmd/vm.rs` | ✅ Corrigido — `valid_vm_name` também no topo de `generate_seed_iso` |
| 4 | kubeconfig cluster-admin em `/tmp` modo 0644 | Qualquer utilizador local no host do control-plane lia credenciais cluster-admin | `cmd/cluster.rs` | ✅ Corrigido — `sudo cat` para stdout do SSH, nunca toca em disco remoto; cópia local a 0600 |
| 5 | `safe_join` do build é só léxico | Symlink na imagem/contexto reabria leitura/escrita arbitrária de ficheiros do host | `cmd/build.rs` | ✅ Corrigido — `confine_to` canonicaliza e confirma confinamento, com teste de regressão |
| 6 | Socket de gestão sem autenticação de peer | Sem `SO_PEERCRED`/chmod — condições comuns davam `container exec` = execução arbitrária em qualquer container a qualquer processo local | `delonix-mgmt/src/lib.rs` | ✅ Corrigido — 0600 + `SO_PEERCRED`, e o mesmo fix aplicado ao socket do `delonix-cri` |

Validado com `cargo build`/`test`/`clippy --workspace` limpos e um teste de fumo ao vivo do socket de gestão (modo 0600, cliente do mesmo utilizador continua a funcionar). **Isto NÃO substitui uma 2.ª auditoria adversarial independente** — os fixes foram escritos e testados por quem os corrigiu, não confirmados por um céptico de fora, ao contrário do processo com que a auditoria original tratou os 24 achados.

**Ainda em aberto (não são HIGH confirmados, mas por resolver antes de confiança total):**
- O núcleo de syscalls (`delonix-runtime/src/lib.rs`, **104 blocos `unsafe`**: `clone`/`mount`/`setns`/seccomp) **nunca teve revisão adversarial** — a auditoria bateu no limite de sessão antes de o cobrir. É o ponto de maior risco por cobrir do repositório.
- **11 achados candidatos por verificar**, incluindo mais um HIGH (`container run --rm` deixa o rootfs inteiro no disco em rootless, mesmo padrão do incidente de disk-pressure já documentado) e um "egress global apaga silenciosamente as políticas por-rede".

---

## 2. Gaps

### 2a. BLOQUEANTES

| Feature | Docker/Podman tem | delonix | Evidência |
|---|---|---|---|
| **`--format` (Go template)** | ps/inspect/info com `--format '{{json .}}'`/`{{.Names}}` — scripts e o próprio `kind` dependem disto | **Ausente** — nenhuma flag de formato; inspect emite JSON fixo | `grep long="format"` em `crates/delonix-runtime-bin/src/` = 0; `cmd_inspect` container.rs:2394 |
| ~~**Multi-stage build** (`FROM…AS x` + `COPY --from`)~~ | Total; é a norma de quase todo o Dockerfile de produção | ✅ **FEITO (2026-07-23)** — cada estágio ganha o seu próprio container/rootfs; `COPY --from=<nome-ou-índice>` lê do estágio já construído; `FROM <estágio-anterior>` clona via `cp -a --reflink=auto`. Gap conhecido: no modo root (overlay), o estágio FINAL ainda tem de ser uma imagem real (sem lineage OCI para um estágio clonado) — erro claro, não falha silenciosa | cmd/build.rs (`build_one_stage`/`resolve_stage_base`/`clone_rootfs`) |
| **BuildKit/buildx** (`RUN --mount=secret/ssh/cache`, heredocs, `--platform`, `--cache-from/to`) | docker buildx / buildah | **Ausente** — sem qualquer flag de segredo/cache/plataforma | `BuildArgs` só context/file/tag, cmd/build.rs:26-37 |
| **Docker Engine API (`/v1.4x` docker-compatível)** | docker.sock e `podman system service` expõem a MESMA API — é o que faz docker CLI/compose/testcontainers falarem via `DOCKER_HOST` | **Ausente** — API é schema próprio `/v1/...` para o control-plane privado | delonix-mgmt/src/lib.rs:100-148; grep `docker.sock`/`containers/json` = 0 |
| **Ler `docker-compose.yml`** | docker compose / podman-compose nativos | **Ausente** — só manifesto próprio `delonix.io/v1` | main.rs sem subcomando Compose; grep 'compose' só apanha `compose_command` |
| **`depends_on` com condições** (`service_healthy`/`service_started`) | compose ordena arranque e espera saúde — essencial app-espera-DB | **Ausente** — `stack apply` cria todos num passo, sem ordenação; `kind: Dependency` é firewall L4, não ordenação | ContainerSpec sem `dependsOn`; cmd/stack.rs:311-350 |
| **Teardown do stack como unidade** (`down`/`stop`/`logs`/`ps` scoped) | `docker compose down` remove tudo do projecto | **Ausente** — StackCmd só Init/Apply/Ls/Describe/Validate; sem registo de projecto | cmd/stack.rs:16-78, 52-57 |
| **testcontainers / CI via `DOCKER_HOST`** | Falam a Engine API contra docker/podman rootless | **Ausente** — consequência da falta de API docker-compatível | delonix-mgmt/src/lib.rs:100-148 |
| **Passagem de GPU real (CDI/nvidia-container-toolkit)** | `--gpus all` / `--device nvidia.com/gpu=all` montam libnvidia-*, nvidia-smi, ldcache — sem isto CUDA não corre | **Ausente** — `--gpus` só faz bind dos nós `/dev/nvidia*`; zero injecção de driver | container.rs:844-865; grep CDI/nvidia-container/libnvidia = 0 |

> **Nota:** três dos bloqueantes (Engine API, compose, testcontainers) são **o mesmo problema-raiz** — ausência de superfície docker-compatível. Resolver a Engine API destrava os outros dois de uma vez.

### 2b. IMPORTANTES

| Feature | Docker/Podman tem | delonix | Evidência |
|---|---|---|---|
| Perfil seccomp custom (`--security-opt seccomp=/x.json`) | Carrega JSON arbitrário | **Silenciosamente ignorado** — só `==unconfined`/`detect`; caminho .json cai no allowlist embutido enquanto o utilizador julga o seu perfil activo | container.rs:1307 grava; lib.rs:3002-3003 só compara strings |
| `container exec` com `-e/-w/-u/--privileged`, `-i` real | Todos suportados | **Parcial** — `-i` é cosmético ("stdin is inherited; the flag keeps CLI parity"); sem -e/-w/-u na assinatura | container.rs:484-495, 2378 |
| `attach` (reanexar stdio) | sim | **Ausente** — só logs dá saída | sem variante Attach, container.rs:190-571 |
| `wait` (bloquear + exit code) | sim (CI/scripts) | **Ausente** — Container nem guarda exit code hoje | sem variante Wait; CLAUDE.md secção spike Kind |
| `kill -s <sinal>` | Qualquer sinal | **Ausente** — só Stop (TERM→KILL fixo) | container.rs:421-428 |
| `restart` subcomando | Para+arranca num comando | **Parcial** — há política `--restart` e Start, mas não subcomando | container.rs:272-277, 414 |
| `logs --tail/--since/--timestamps` | sim | **Parcial** — só `-f`; lê ficheiro inteiro; só containers detached | container.rs:3222-3267 |
| `--net <custom>` em rootless | podman fiável | **Limitação documentada** (mas o re-exec via nsenter já existe — a nota do CLAUDE.md está desactualizada) | infra.rs:2421, container.rs:1403-1425 |
| `--network-alias` | aardvark-dns resolve aliases | **No-op** — gravado e mostrado mas nunca consultado no `dns_resolve` | container.rs:1346; infra.rs:3217 só casa `name` |
| Driver macvlan/ipvlan | Realizado | **Não realizado** em rootless (`Realized=False`, precisa CAP_NET_ADMIN na init-netns) | network.rs:244-250 |
| Overlay multi-nó (forwarding real) | swarm encaminha entre nós | **Parcial** — device+FDB criados; forwarding inter-nó não provado E2E | CLAUDE.md secção overlay |
| Publish com host-IP (`127.0.0.1:8080:80`) | sim | **Rejeitado** — `parse_publish` exige host_port só dígitos | lib.rs:337-357 |
| Backend pasta/passt | Default moderno do podman (mais rápido) | **Ausente** — só slirp4netns, um processo por container publicado | lib.rs:2190-2224 |
| `--ip` fixo | sim | **Recusado** — IPAM por hash do id | container.rs:1360-1364 |
| Storage NFS/CIFS/WebDAV em rootless | Também precisam de privilégio | **Parcial** — validado E2E mas exige CAP_SYS_ADMIN; rootless puro falha | delonix-volume/src/lib.rs:182-226 |
| Sintaxe `--mount type=...` | docker+podman (recomendada) | **Ausente** — só `-v` e `--tmpfs` | container.rs:252-254 |
| Opções de bind além de `:ro` (`:z/:Z` SELinux, `:U`, propagação) | Críticas em RHEL/Fedora SELinux enforcing | **Silenciosamente ignoradas** — 3.º campo só reconhece `ro` | delonix-volume/src/lib.rs:516-524 |
| `volume inspect` / `network inspect` em JSON | sim | **Parcial** — só texto PT; migrar seria breaking change | cmd/volume.rs:253-265 |
| AppArmor por omissão | docker-default automático | **Ausente** — só com `--apparmor` explícito | container.rs:1304 |
| userns `keep-id/auto/nomap` | Essencial p/ posse em bind mounts rootless | **Ausente** — só booleano; ficheiros aparecem com subuids altos | container.rs:327-331; lib.rs:1336-1382 |
| ~~`--build-arg` / `ARG`~~ | Quase todo o CI | ✅ **FEITO (2026-07-23)** — substituição `${NAME}`/`$NAME`, incluindo antes do 1º `FROM` | delonix-image/build.rs (`parse_dockerfile_with_args`) |
| ~~Cache de layers / rebuild incremental~~ | Por instrução | ✅ **FEITO (2026-07-23), rootless** — cadeia de hash por instrução, clonagem via `cp -a --reflink=auto`; modo root nunca usa cache (ver nota na secção 2a) | cmd/build.rs (`build_one_stage`, `try_clone_cached`) |
| ~~ENTRYPOINT/USER preservados no build rootless~~ | Sempre gravados | ✅ **FEITO (2026-07-23)** — ambos sobrevivem ao commit rootless agora | delonix-image/build.rs (`commit_flat_rootfs*`) |
| save/load/import de imagem (air-gap) | sim | **Parcial** — `load_docker_archive` existe mas não ligado à CLI; `export` produz bundle runc, não tar portátil | delonix-image/src/load.rs; cmd/image.rs:626-652 |
| Recriar serviço em drift de config | `compose up` compara e substitui | **Parcial** — idempotência só por nome; mudar imagem/porta e re-aplicar é no-op | cmd/manifest.rs:8 |
| Healthcheck declarativo a gatear arranque/restart | `healthcheck:` no compose | **Parcial** — só da imagem, corre sob pedido; restart por exit code, nunca por saúde | container.rs:2470-2489, 1794-1890 |
| Quadlet / units declaráveis versionáveis | podman generate systemd / Quadlet | **Parcial** — `boot enable` fotografa containers vivos, não é ficheiro declarativo | boot.rs:131-135 |
| Auto-update de imagens | podman auto-update + timer | **Ausente** | grep autoupdate = 0 |
| `--pids-limit` configurável | por container | **Ausente** — fixo em 512 | lib.rs:2205 |
| cpuset/cpu.weight/io.weight no rootless-delegado (o normal) | podman aplica no cgroup delegado | **Ignorados** — só escritos no caminho não-delegado (root); delegado só faz memory/pids/cpu.max | lib.rs:2708-2710, 2796 |
| `container update --memory/--cpus` em rootless-delegado | Reescreve o cgroup real | **No-op silencioso** — escreve num leaf que não existe no modo delegado | lib.rs:4274-4283 vs leaf real em 2677/2712 |
| Limites garantidos em rootless SEM delegação systemd | podman assume Delegate=yes por omissão | **Best-effort** — memory/cpu/pids não aplicados; fork-bomb pode matar o host | lib.rs:2736-2768 |

### 2c. MENORES

| Feature | delonix | Evidência |
|---|---|---|
| `stats` em stream contínuo | Só uma amostra (dash TUI cobre o live) | container.rs:3173-3218 |
| `rename` / `port` (subcomandos) | Ausentes | container.rs:190-571 |
| Portas <1024 em rootless | Auto-rotas forçadas a :8080 (paridade prática c/ podman) | ingress_proxy.rs:498-499 |
| Estabilidade de hostfwd / refcount ingress | Causa externa (delonix-engine privado); reaper morto fail-open + refcount vaza | CLAUDE.md secção "portas morriam" |
| IPAM por hash | Colide por aniversário ~300 containers (mitigado por lease) | lib.rs:469-471 |
| Cloud Native Buildpacks / registo interno | Scaffolding, sem CLI/E2E | buildpack.rs, internal_registry.rs |
| `image prune` dangling / `image inspect` JSON | Só `system prune` global / só describe texto | cmd/system.rs:180-249 |
| Layers de build comprimidos | tar não-comprimido (incha o registo, válido OCI) | build.rs:456 |
| Base CVE fiável por omissão | 5 entradas placeholder; precisa `scan --update` | cmd/scan.rs:7-21 |
| Volume driver plugins de terceiros | Conjunto de drivers fechado | delonix-volume/lib.rs:131-172 |
| Auto-criação de dir de bind inexistente | Erra em vez de criar | delonix-volume/lib.rs:533-534 |
| Quota dura por-volume em rootless | Só monitor (cap duro só em root) | delonix-volume/lib.rs:338-342 |
| MCS SELinux automático / NNP desligável / `--security-opt label=,mask` | Ausentes | lib.rs:1615-1616; container.rs:1305-1311 |
| `--memory-swap/reservation/swappiness/oom-kill-disable` | Ausentes (swap fixo a 0) | lib.rs:2789 |
| GPU selectiva (count/device index) / `--device` de bloco / io.max por container | Ausentes/por desenho | container.rs:845-846; lib.rs:1119-1127; lib.rs:2333-2338 |
| `podman play kube` / `kind: Pod` no manifesto / escopo de projecto | Só `kube generate`; pods só imperativos | cmd/kube.rs; stack.rs:113 |
| API: eventos por polling / logs-exec não-streaming / sem TCP+TLS | Sem daemon (polling); API só request/response e unix socket | system.rs:578-591; delonix-mgmt/lib.rs:119-121 |

---

## 3. Diferenciais do delonix (o que faz melhor/diferente)

Honestamente, não é só "Docker com menos features" — há genuíno valor novo:

- **Reconfiguração a quente sem parar o container** (`container update`) — muda portas/volumes/redes/banda com o **PID inalterado**. O dataplane não pertence ao ciclo de vida do processo; no Docker mudar uma porta obriga a recriar. (container.rs:507-549)
- **Daemonless real** — não há dockerd/podman-service; cada comando actua directamente, infra (holder/slirp/proxy) sobe on-demand só quando há rede/carga. Persistência no boot via systemd. É o modelo do Podman, provado. (system.rs:721-753)
- **Um só motor: container + microVM + Kubernetes** — `delonix vm` (Cloud Hypervisor/libvirt), servidor CRI para kubelet real (substitui containerd/CRI-O), imagem VM dourada e `cluster kubeadm`. Ninguém no espaço Docker/Podman cobre este arco.
- **Segurança mais estrita por omissão** — no-new-privs **sempre** ligado, e uma **verificação fail-closed** que lê `/proc/self/status` e aborta se seccomp/caps/NNP não vigoram — garantia que docker/podman **não** dão. (lib.rs:706-757)
- **Assinatura cosign/sigstore + scan de CVE + SBOM embutidos** no próprio motor de imagens, sem trivy/grype externos. (sign.rs, scan.rs)
- **Rede rootless acima do podman** — overlay VXLAN+WireGuard rootless (docker exige swarm; podman não tem overlay rootless nativo), egress/namespace firewall dirigido (`kind: Dependency`), shaping de banda por container, DNS de descoberta com isolamento por namespace.
- **Storage de rede estilo PersistentVolume** — NFS/CIFS/WebDAV como volume nomeado montável, validado E2E com NAS real. (delonix storage)
- **Snapshots e quota por-volume** — tar crash-consistente rootless-safe + cap por loopback ext4. Docker CLI puro não tem.
- **describe estilo kubectl** (aditivo ao inspect), **healthcheck/ssh/dash TUI** como extras de operação.
- **Limites obrigatórios** — o arranque falha se o cgroup não aplicar o limite (Docker por omissão não limita nada).
- **i18n** — fonte EN + catálogo gettext pt.po embutido, help do clap traduzido em runtime.
- **Pods reais multi-container** (`kind: Pod` / `delonix pod`) — N containers a partilhar netns+IPC+UTS como um Pod do k8s, validado E2E (2026-07). Nenhum destes dois concorrentes tem isto fora do próprio k8s.
- **`kindest/node` (Kind) a arrancar sem Docker** — cgroup2, netfilter (nft) e containerd resolvidos em rootless; um control-plane Kubernetes v1.34 completo ficou `Ready` a correr sobre o Delonix, com o kube-proxy a programar netfilter no nosso netns. Prova viva do "container+VM+k8s num só motor".
- **`vm bridge`** (experimental, opt-in, privilegiado) — VM libvirt e container comunicam por IP directo, sem SNAT, com firewall por-container a continuar a valer. Fecha a única lacuna que o modelo rootless não fazia sozinho.
- **Diagnóstico automático de crash** — `container describe`/`ls` mostram a RAZÃO (`process_gone`/`pid_reused`) e a hora de um `Crashed`, com um snapshot forense (tail do log) gravado automaticamente; `container start` volta a supervisionar `--restart` mesmo que o supervisor anterior tenha morrido com o host. Nem docker nem podman expõem esta razão — só "Exited"/"Dead".

---

## 4. Roadmap priorizado para paridade de produção

**Fase 0 — SEGURANÇA, antes de qualquer exposição pública (bloqueia tudo o resto):**
- ✅ **FEITO (2026-07-23)**: os 6 HIGH da auditoria (secção 1a) — path traversal no whiteout OCI, IDs do CRI, nome de VM em `generate_seed_iso`, kubeconfig em `/tmp`, symlink no `COPY` do build, socket de gestão sem `SO_PEERCRED`.
- **2.ª auditoria adversarial independente** para confirmar os 6 fixes de fora para dentro (não foram revistos por um céptico, ao contrário do processo que os encontrou).
- **1.ª auditoria do núcleo de syscalls** (`delonix-runtime/lib.rs`, 104 `unsafe`) e `delonix-net/infra.rs` — nunca tiveram revisão adversarial nenhuma. E triar os 11 achados candidatos (inclui mais um HIGH: fuga de rootfs no `--rm` rootless).

**Fase 1 — destrava o ecossistema (maior alavanca, um investimento resolve três bloqueantes):**
1. **Shim da Docker Engine API** sobre unix socket (`/containers/json`, `/create`, `/images/json`, `/version`, `/info`, `/events`) — destrava docker CLI, `docker compose`, testcontainers e CI via `DOCKER_HOST` de uma vez. Combina com o `--format` abaixo, já que a API precisa da mesma serialização.
2. **`--format` / Go-template** em ps/inspect/info — bloqueante isolado para scripting. O modo Kind já não precisa disto para arrancar (resolvido — ver Diferenciais), mas continua útil para scripting/CI em geral.

**Fase 2 — build de produção:**
3. ✅ **FEITO (2026-07-23)**: multi-stage build (`FROM…AS` + `COPY --from`) — ver secção 2a.
4. ✅ **FEITO (2026-07-23)**: `--build-arg`/`ARG` (com `${NAME}`/`$NAME`, incluindo antes do 1º `FROM`) + `USER`/`ENTRYPOINT` já sobrevivem ao commit rootless (antes só o `ENTRYPOINT` do modo root sobrevivia; `USER` perdia-se sempre, em ambos os modos, e nem chegava ao JSON de config OCI). **Gap novo, separado, encontrado ao validar**: `container run` nunca lê o `User` guardado na imagem para definir o uid em runtime — só um `--user` explícito o faz. Guardar o `USER` (feito) e aplicá-lo automaticamente no `run` são features distintas; a resolução de nome→uid (`resolve_run_user`) já existe, falta só o default.
5. ✅ **FEITO (2026-07-23), rootless**: cache de layers por instrução (`--no-cache` para saltar) — ver secção 2a/2b. Modo root continua sem cache (executa sempre a sério — ver a nota na secção 2a sobre `commit_upper` precisar de um `upper/` real).
6. **BuildKit-lite** — pelo menos `RUN --mount=type=secret` e `--platform` (segredos de build e cross-compile são o mínimo de CI serio). Único item por fazer nesta fase.

**Fase 3 — compose e orquestração local (se o alvo for substituir compose):**
7. **Parser de `docker-compose.yml`** + **`depends_on` com `condition: service_healthy`** + **healthcheck declarativo a gatear arranque** + **`stack down`/`logs`/`ps` scoped a projecto**. (Alternativamente, o shim da Fase 1 já deixa o `docker compose` real falar com o motor — pode tornar 7 desnecessário.)

**Fase 4 — correcções de correctude silenciosas restantes:**
- ✅ **FEITO**: perfil seccomp custom (erro explícito), opções de bind `:z/:Z` SELinux (erro explícito), `--network-alias` no-op (agora avisa).
8. **`container update --memory/--cpus` no-op silencioso em rootless-delegado** + **cpuset/weights ignorados no delegado** — ainda por corrigir; precisa de teste num host com delegação systemd real. (lib.rs:4274)

**Fase 5 — paridade de CLI de operação:**
9. `wait` (+ guardar exit code real — hoje só há `crash_reason` best-effort, nunca um exit code capturado, porque o motor não é o pai real do processo), `kill -s`, `attach`, `restart` (subcomando dedicado), `logs --tail/--since`, `exec -e/-w/-u`, `rename`, `port`.

**Fase 6 — rede/GPU/recursos avançados:**
10. **GPU real via CDI/nvidia-container-toolkit** — bloqueante só para o segmento GPU, mas total nesse segmento; grande esforço (injecção de libs de driver).
11. Publish com host-IP, backend pasta/passt (perf), `--ip` fixo, macvlan/ipvlan rootless (limitado por CAP_NET_ADMIN), `--pids-limit`, tuning de memória/swap.

**Racional da ordem:** a Fase 0 tem a maior razão valor/esforço — a maioria do "não serve em produção" vem de **incompatibilidade de superfície**, não de falta de capacidade de kernel (onde o motor já está a par ou à frente). A Fase 3 é barata e deve entrar cedo porque são **falhas silenciosas de segurança/correctude** — piores que uma feature em falta, porque o utilizador julga que está protegido.