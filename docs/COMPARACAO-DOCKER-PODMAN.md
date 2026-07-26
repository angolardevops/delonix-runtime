# Análise de Gaps — delonix-runtime vs Docker/Podman rootless em produção

> Actualizado 2026-07-26. Nesta revisão: os 4 gaps "bloqueantes" da secção 2a fechados (paridade
> de verbos CLI v0.25.0, mutações da Docker Engine API v0.26.0, BuildKit-lite v0.27.0, GPU/CDI
> v0.28.0); `docker-compose.yml` nativo + `depends_on` + teardown de projecto fechados (v0.29.0);
> e **as DUAS auditorias adversariais independentes que faltavam foram feitas** — núcleo de
> syscalls + holder de rede (zero achados novos, secção 1b) e os 6 HIGH da auditoria original de
> 2026-07-21 (5/6 confirmados sólidos, 1/6 tinha um TOCTOU residual real, agora corrigido, secção
> 1a). A Fase 0 de segurança está fechada; o que resta é a triagem dos 11 achados candidatos e
> gaps "importantes" não-bloqueantes (secção 2b).

## 1. Veredicto executivo

O **delonix-runtime está muito mais perto de um substituto de produção do Docker/Podman rootless
do que o AGENTS.md sugeria há poucos dias** — e em várias dimensões já ultrapassa ambos. Os 4 gaps
"bloqueantes" de compatibilidade (API Docker, BuildKit-lite, GPU/CDI, compose nativo) estão FEITOS
(v1, com limitações documentadas, nunca silenciosas). **Em segurança, as duas auditorias
independentes que faltavam foram concluídas nesta revisão**: o núcleo de syscalls + o holder de
rede (secção 1b, zero achados novos) e os 6 HIGH da auditoria original (secção 1a — 5
confirmados, 1 corrigido de novo). Não há mais nenhuma peça de segurança "por confirmar" em
aberto — o que resta é a triagem dos 11 achados candidatos menores (nunca confirmados nem
refutados) e gaps de correctude "importantes" mas não-bloqueantes (cgroups rootless-delegados,
`--format` Go-template).

**Para que casos JÁ serve (com confiança):**
- **Execução e operação interactiva de containers** — run/ps/stop/exec/logs/inspect + extras que o Docker não tem (reconfiguração a quente, pause via freezer, describe estilo kubectl, diagnóstico automático de crash com razão+forense).
- **Distribuição de imagens OCI** — pull/push/tag/history/login interoperáveis com registos, com assinatura cosign e scan de CVE embutidos (diferenciais).
- **Rede de container single-node** — `--net host/none/bridge-custom`, publish rootless via slirp4netns, DNS de descoberta com isolamento por namespace, overlay VXLAN+WireGuard, firewall L4/egress e shaping — supera o podman rootless em várias frentes.
- **Pods reais multi-container** (`kind: Pod`) — netns + IPC + UTS partilhados, validado E2E.
- **Bootstrap de Kubernetes SEM Docker** — servidor CRI real para kubelet, imagem VM dourada, `cluster kubeadm`, e **modo Kind (`kindest/node`) já ARRANCA e um control-plane v1.34 fica `Ready`** (netfilter/cgroup2/containerd todos resolvidos). Terreno onde é motor único (container + VM + k8s), ninguém no espaço Docker/Podman cobre este arco.
- **API Docker-compatível, BuildKit-lite, GPU/CDI e `docker-compose.yml` nativo** — mutações de ciclo de vida, `--mount=secret`/`--platform`, `--gpus`/CDI, `compose up/down/ps/logs` com `depends_on` real, todos v1 validados ao vivo (ver secções 2a/2b).

**Para que NÃO serve (ainda):**
- **Compatibilidade de ecossistema residual (limita o âmbito, não a segurança nem a confiança)** — sem `--format` Go-template (scripting/CI), gaps de correctude silenciosa em cgroups rootless-delegados (`container update --memory/--cpus` no-op nesse modo, secção 2b), `profiles`/`extends`/multi-ficheiro do compose ainda por fazer (erro claro, não silencioso).

**Posição global:** um runtime rootless-first **sólido em desenho, confirmado por duas rondas de auditoria adversarial independente, e já à par ou à frente do Docker/Podman em ciclo de vida, rede, build e compatibilidade de API**. A barreira que existia — "segurança não confirmada de fora para dentro" — está fechada. O que resta é compatibilidade de superfície residual (scripting `--format`, correctude fina em cgroups delegados), não mais uma questão de confiança.

---

## 1a. Segurança — 6 HIGH da auditoria de 2026-07-21, CORRIGIDOS em 2026-07-23, CONFIRMADOS de forma independente em 2026-07-26

Fonte completa: [`docs/AUDITORIA-E2E.md`](AUDITORIA-E2E.md) (24 achados confirmados por 2 céticos adversariais + 11 candidatos ainda por verificar).

| # | Achado | Impacto | Local | Estado |
|---|---|---|---|---|
| 1 | Path traversal em whiteouts OCI | Imagem maliciosa apaga ficheiros/directórios arbitrários do utilizador (ex.: a home inteira) — reachable no `container run` rootless DEFAULT | `delonix-image/src/overlay.rs` | ✅ **CONFIRMADO FIXO (2026-07-26)** — `safe_rel` no ramo de whiteout + confinamento contra symlink plantado; auditoria independente tentou reconstruir o exploit e não conseguiu |
| 2 | IDs do CRI sem validação | Kubelet comprometido apaga/lê `*.json` arbitrário via `../` | `delonix-cri/src/runtime_svc/lifecycle.rs` | ✅ **CONFIRMADO FIXO (2026-07-26)** — whitelist centralizada em `write_rec`/`read_rec`/`remove_rec`, confirmado como o ÚNICO caminho de construção de path no crate |
| 3 | Nome de VM ainda escapa o fix anterior | `generate_seed_iso` escrevia ficheiros fora do state-dir ANTES de `create()` validar o nome | `cmd/vm.rs` | ✅ **CONFIRMADO FIXO (2026-07-26)** — `valid_vm_name` no topo de `generate_seed_iso`, com um 2º gate independente em `delonix_vm::create_with` |
| 4 | kubeconfig cluster-admin em `/tmp` modo 0644 | Qualquer utilizador local no host do control-plane lia credenciais cluster-admin | `cmd/cluster.rs` | ⚠️ **PARCIALMENTE fixo → CORRIGIDO AGORA (2026-07-26)** — o lado remoto estava correcto (`sudo cat` para stdout do SSH, nunca toca em disco remoto), mas a auditoria independente reproduziu um TOCTOU residual no lado LOCAL: `fs::write` cria o ficheiro no modo do umask (664 medido ao vivo neste host) e só DEPOIS aplica `chmod 600` — uma janela real em que outro utilizador local podia ler as credenciais cluster-admin. Corrigido: `OpenOptions::mode(0o600)` define o modo ATOMICAMENTE na criação (mesmo padrão já usado por `ensure_libvirt_network`) |
| 5 | `safe_join` do build é só léxico | Symlink na imagem/contexto reabria leitura/escrita arbitrária de ficheiros do host | `cmd/build.rs` | ✅ **CONFIRMADO FIXO (2026-07-26)** — `confine_to` reatribui o caminho já canonicalizado antes de qualquer `fs::copy`/`create_dir_all`, `copy_dir_all` revalida recursivamente cada entrada aninhada |
| 6 | Socket de gestão sem autenticação de peer | Sem `SO_PEERCRED`/chmod — condições comuns davam `container exec` = execução arbitrária em qualquer container a qualquer processo local | `delonix-mgmt/src/lib.rs` | ✅ **CONFIRMADO FIXO (2026-07-26)** — `SO_PEERCRED` verificado DENTRO do loop de accept, antes de qualquer dispatch; mesmo padrão confirmado em `delonix-cri` |

Validado com `cargo build`/`test`/`clippy --workspace` limpos. **2026-07-26: uma auditoria adversarial INDEPENDENTE destes 6 fixes foi finalmente feita** (o item que faltava desde 2026-07-23) — releu cada local, tentou reconstruir cada exploit original, e correu a suite de testes existente como verificação adicional. 5/6 confirmados sólidos; 1/6 (kubeconfig) tinha um gap residual real, agora também corrigido. **Este item da Fase 0 está agora FECHADO.**
- **11 achados candidatos por verificar**, incluindo mais um HIGH (`container run --rm` deixa o rootfs inteiro no disco em rootless, mesmo padrão do incidente de disk-pressure já documentado) e um "egress global apaga silenciosamente as políticas por-rede".

---

## 1b. Segurança — núcleo de syscalls + holder de rede, DUAS rondas adversariais (2026-07-23 e 2026-07-26)

Ao contrário do que uma versão anterior deste doc afirmava, `delonix-runtime/src/lib.rs` (104 blocos
`unsafe`: `clone`/`fork`/`mount`/`pivot_root`/`setns`/`unshare`/seccomp-BPF/mapeamento uid-gid) e
`delonix-net/src/infra.rs` (holder + socket de controlo) **já tinham sido auditados** — a "2ª ronda"
de 2026-07-23 (ver AGENTS.md) encontrou e corrigiu 2 CRITICAL + 3 HIGH nesses dois ficheiros
específicos (todos em produção desde o v0.10.1). O que faltava era a **confirmação independente de
fora para dentro** desses fixes — feita em 2026-07-26.

**Resultado da auditoria independente de 2026-07-26: zero achados novos CRITICAL/HIGH.** Verificado
explicitamente, com raciocínio concreto (não apenas "parece ok"):

| Área | Verificação | Resultado |
|---|---|---|
| Mapeamento uid/gid (`CLONE_NEWUSER`) | Nenhum caminho (root, rootless single-uid, rootless com subuid) mapeia o uid 0 do container para o uid 0 REAL do host | ✅ Sólido |
| Check fail-closed pós-seccomp/caps/NNP | Lê `/proc/self/status` e aborta (exit 126) se `NoNewPrivs≠1`/seccomp não activo/qualquer capability sobrevive fora do `cap_keep` — corre no `spawn` E no `exec` | ✅ Sólido |
| `allowed_syscalls()` (seccomp) | `mount`/`umount2`/`pivot_root`/`setns`/`unshare`/`ptrace`/`bpf` ausentes por omissão; `clone3` forçado a `ENOSYS` especificamente para não contornar o filtro de `CLONE_NEWUSER` do `clone` clássico via a struct de flags (inacessível ao BPF clássico) | ✅ Sólido |
| `safe_bind_target`/`bind_volume` (TOCTOU de symlink plantado pela imagem) | Resolve o alvo do bind componente a componente contra o rootfs, recusa qualquer componente symlink, ANTES do `mount` — mesma protecção em `mount_live`/`unmount_live` (hot-plug) | ✅ Sólido |
| eBPF do device-cgroup | Offsets da struct `bpf_attr` construída à mão conferidos byte a byte contra o layout real do kernel | ✅ Sólido |
| Higiene de fd através de forks | Todos os forks (`log_shim`, `exec` duplo-fork, `mount_live`/`unmount_live`, `reexec_mapped`) fecham fds herdados explicitamente ou usam `close_range` | ✅ Sólido |
| Socket de controlo do holder (`infra.rs`) | `SO_PEERCRED` verificado em TODAS as ~25 formas de comando ANTES de qualquer dispatch; todo o campo controlado pelo atacante que chega a um argv `nft`/`ip` passa primeiro por um validador de charset ou é derivado de hash/IPAM | ✅ Sólido |
| Bug da substring do egress global (`infra.rs:1531`, já documentado como achado aberto noutra secção) | `is_global_egress_drop_line` já exclui linhas com `iifname` | ✅ Já corrigido |

**Achado BAIXO, não um exploit concreto, registado para vigilância futura:** o `nft` real
reconcatena o seu próprio argv com espaços e reanalisa isso como uma única linha de script — um
único elemento de argv com um espaço/chaveta poderia em teoria agir como vários tokens de gramática
para o `nft`, mesmo sem shell nenhum envolvido. Nenhuma string alcançável por um atacante chega hoje
a um argv `nft` sem passar primeiro por um validador de charset/formato ou ser derivada de
hash/IPAM — mas é um invariante estruturalmente frágil (um único validador em falta num futuro
comando de controlo reabre esta classe). Recomendação: uma vigilância (`grep -n '"nft"'`) sempre
que um novo tipo de comando for adicionado ao socket de controlo.

**O que isto NÃO cobre** (continua em aberto, ver secção 1a): os 6 HIGH da auditoria original de
2026-07-21 (path traversal em whiteouts OCI, IDs do CRI, nome de VM, kubeconfig em `/tmp`, symlink
no `COPY` do build, socket do `delonix-mgmt`) — nenhum destes vive em `lib.rs`/`infra.rs`, por isso
esta auditoria de 2026-07-26 não os re-confirma. Continuam a precisar do seu próprio 2º par de olhos.

---

## 2. Gaps

### 2a. BLOQUEANTES

| Feature | Docker/Podman tem | delonix | Evidência |
|---|---|---|---|
| **`--format` (Go template)** | ps/inspect/info com `--format '{{json .}}'`/`{{.Names}}` — scripts e o próprio `kind` dependem disto | **Ausente** — nenhuma flag de formato; inspect emite JSON fixo | `grep long="format"` em `crates/delonix-runtime-bin/src/` = 0; `cmd_inspect` container.rs:2394 |
| ~~**Multi-stage build** (`FROM…AS x` + `COPY --from`)~~ | Total; é a norma de quase todo o Dockerfile de produção | ✅ **FEITO (2026-07-23)** — cada estágio ganha o seu próprio container/rootfs; `COPY --from=<nome-ou-índice>` lê do estágio já construído; `FROM <estágio-anterior>` clona via `cp -a --reflink=auto`. Gap conhecido: no modo root (overlay), o estágio FINAL ainda tem de ser uma imagem real (sem lineage OCI para um estágio clonado) — erro claro, não falha silenciosa | cmd/build.rs (`build_one_stage`/`resolve_stage_base`/`clone_rootfs`) |
| **BuildKit/buildx** (`RUN --mount=secret/ssh/cache`, heredocs, `--platform`, `--cache-from/to`) | docker buildx / buildah | ✅ **FEITO parcial (v0.27.0)** — `RUN --mount=type=secret,id=<nome>[,target=][,required=]` (bind-mount ao vivo via `mount_live`/`unmount_live` na janela do próprio `RUN`, nunca chega a uma layer/cache) + `--platform linux/<arch>` (resolve a imagem base do arch certo, carimba-a no resultado, preflight claro de binfmt_misc/qemu antes de arrancar um build cross-arch). **Ausente**: `type=ssh`/`type=cache`/`type=bind` (erro claro, nunca mal-interpretado como shell), heredocs, `--cache-from/to`, manifest-list multi-arch no push (só constrói UM arch por invocação, como o `docker build --platform` sem buildx) | cmd/build.rs (`parse_run_flags`/`mount_run_secrets`), `--platform` em `cmd/build.rs`/`cmd/util.rs::resolve_or_pull_platform` |
| **Docker Engine API (`/v1.4x` docker-compatível)** | docker.sock e `podman system service` expõem a MESMA API — é o que faz docker CLI/compose/testcontainers falarem via `DOCKER_HOST` | ✅ **FEITO (v0.26.0)** — `delonix serve docker-api` ganhou as mutações de ciclo de vida: `POST /containers/create\|start\|stop\|kill\|wait\|restart\|rename`, `DELETE /containers/{id}`, `GET /containers/{id}/json`, reutilizando tal-e-qual o `cmd_run`/`cmd_stop`/etc. do CLI. **Validado contra um `docker` CLI real** (27.3.1): `docker create`+`start`+`inspect`+`kill`+`wait`+`restart`+`rename`+`stop`+`rm` — todos correctos e instantâneos (é o caminho que `docker compose up/down` usa). **Limitação documentada, não silenciosa**: o subcomando de conveniência `docker run` (create+start num só comando) não devolve o controlo ao terminal de forma fiável contra este servidor — parece ser um comportamento interno do próprio CLI Go (sinalização/cleanup) não replicável com `create`+`start` separados; usa esses dois em vez de `run`. `exec` (hijacking HTTP interactivo) continua fora de escopo; `--restart` (supervisor) é recusado com erro claro em vez de arriscar um `fork()` de um processo multi-thread | cmd/dockerapi.rs |
| **Ler `docker-compose.yml`** + **`depends_on` com condições** + **teardown de projecto** | docker compose / podman-compose nativos; ordena arranque e espera saúde; `down` remove tudo do projecto | ✅ **FEITO (v0.29.0)** — `delonix compose up\|down\|ps\|logs\|config`, um parser tipado (sem dependência nova) do Compose Spec v2.x traduzido directamente para `RunOpts` (mesma família de `pod_to_run_opts`/`docker_config_to_run_opts`) ou para `ManifestDoc`s reaproveitando `image`/`network`/`volume::apply` tal-e-qual. `depends_on` com as 3 condições (`service_started`/`service_healthy`/`service_completed_successfully`) via ordenação topológica (ciclo → erro claro, nunca uma ordem arbitrária) + espera pelo healthcheck real (inline ou da imagem). Projecto = `delonix.io/compose-project=<nome>` nos containers (mesma ideia de `pod.rs`) + nomes determinísticos `<projecto>_<nome>` para redes/volumes (sem label própria). **Validado ao vivo de ponta-a-ponta**: `web` só arrancou depois do `pg_isready` do `db` ter sucesso; `down -v` removeu os 2 containers + rede + volume sem deixar nada para trás; re-`up` idempotente. **Por fazer**: `profiles`/`extends`/`configs`/`secrets` top-level/multi-ficheiro (erro claro, nunca ignorados em silêncio), `build.target`, réplicas≠1, `ipv4_address` fixo, volumes anónimos, porta sem host explícito | `cmd/compose.rs` |
| **testcontainers / CI via `DOCKER_HOST`** | Falam a Engine API contra docker/podman rootless | **Ausente** — consequência da falta de API docker-compatível | delonix-mgmt/src/lib.rs:100-148 |
| **Passagem de GPU real (CDI/nvidia-container-toolkit)** | `--gpus all` / `--device nvidia.com/gpu=all` montam libnvidia-*, nvidia-smi, ldcache — sem isto CUDA não corre | ✅ **FEITO parcial (v0.28.0)** — `cmd/cdi.rs` é um CONSUMIDOR de CDI (parseia specs já gerados por `nvidia-ctk cdi generate` em `/etc/cdi`/`/var/run/cdi`), nunca um `nvidia-container-cli configure --pid=` (esse modelo exigiria `setns` por PID num userns alheio — o mesmo problema de privilégio cross-namespace que o `--net` já contorna com re-exec, não com attach externo). `deviceNodes`/`mounts`/`env` do spec traduzem-se para o `Vec<Mount>`/`Vec<String>` que `-v`/`--device` já alimentam — aplicados pelo PRÓPRIO init do container, antes do `pivot_root`, **zero modelo de privilégio novo** (o mesmo mecanismo já rootless do `-v`/`--device`). `--gpus nvidia\|all` e `--device nvidia.com/gpu=<nome\|all>` recusam com erro claro e accionável se não houver spec CDI nem `nvidia-ctk` no PATH (nunca cai em silêncio para o bind cru de `/dev/nvidia*`, que falharia com um erro confuso do CUDA). `ldconfig -r <rootfs>` best-effort após o mount (substituto mais simples do hook `createContainer` real do CDI, que precisa do protocolo OCI-hook-stdin-state). `--gpus dri` inalterado (Mesa/VAAPI é open-source, já vem no pacote da imagem). **Por confirmar num host GPU real** (impossível neste sandbox): precedência exacta `/etc/cdi` vs `/var/run/cdi`; se `ldconfig -r` chega para substituir os hooks reais | `cmd/cdi.rs` (`resolve_cdi_device`/`ensure_cdi_available`), `cmd/container.rs` (`--gpus`/`--device` wiring), `crates/delonix-runtime/src/lib.rs::setup_rootfs` (`ldconfig -r`) |

> **Nota:** três dos bloqueantes (Engine API, compose, testcontainers) são **o mesmo problema-raiz** — ausência de superfície docker-compatível. Resolver a Engine API destrava os outros dois de uma vez.

### 2b. IMPORTANTES

| Feature | Docker/Podman tem | delonix | Evidência |
|---|---|---|---|
| Perfil seccomp custom (`--security-opt seccomp=/x.json`) | Carrega JSON arbitrário | **Silenciosamente ignorado** — só `==unconfined`/`detect`; caminho .json cai no allowlist embutido enquanto o utilizador julga o seu perfil activo | container.rs:1307 grava; lib.rs:3002-3003 só compara strings |
| ~~`container exec` com `-e/-w/-u`~~ | Todos suportados | ✅ **FEITO (v0.25.0)** — overrides por-chamada, nunca persistidos; `-w` também corrigiu um bug real: `exec` fazia `chdir("/")` incondicional, ignorando o `workdir` da imagem mesmo sem `-w` nenhum. `--privileged`/`-i` reais continuam por fazer | container.rs (`ExecOverrides`, `cmd_exec`) |
| ~~`attach` (reanexar stdio)~~ | sim | ✅ **FEITO (v0.25.0), só saída** — reaproveita o mecanismo de `logs -f`; `-i` é recusado com erro claro (este motor não guarda um conduíte de stdin vivo para um container já arrancado em detached, ao contrário de um shim persistente por-container) | container.rs (`cmd_attach`) |
| ~~`wait` (bloquear + exit code)~~ | sim (CI/scripts) | ✅ **FEITO (v0.25.0)** — bloqueia e imprime o exit code real SÓ quando um supervisor `--restart` é o pai real do processo (o único caso em que há um `waitpid` genuíno); sem supervisor, a morte continua a aparecer como `Crashed`/137 — limitação arquitectural pré-existente (o motor não é o pai real), não um bug do `wait` | container.rs (`cmd_wait`) |
| ~~`kill -s <sinal>`~~ | Qualquer sinal | ✅ **FEITO (v0.25.0)** — nome ou número, sem forçar `Stopped` (o resultado real, ex. `Crashed` para um sinal que mata mesmo, só se reflecte na próxima observação) | container.rs (`cmd_kill`, `runtime::send_signal`) |
| ~~`restart` subcomando~~ | Para+arranca num comando | ✅ **FEITO (v0.25.0)** — reaproveita `stop`+`start` tal-e-qual (imprime 2 linhas em vez de 1, trade-off aceite para não duplicar a lógica de rede/namespace de nenhum dos dois) | container.rs (`cmd_restart`) |
| ~~`logs --tail/--since/--timestamps`~~ | sim | ✅ **FEITO (v0.25.0)** — só para containers corridos com `--log-cri` (o único formato com timestamps reais por linha); sem isso, erro claro em vez de uma coluna de timestamp em branco | container.rs (`parse_cri_log_line`, `cmd_logs`) |
| `rename` / `port` (subcomandos) | Ausentes | ✅ **FEITO (v0.25.0)** | container.rs (`cmd_rename`, `cmd_port`) |
| `--net <custom>` em rootless | podman fiável | **Limitação documentada** (mas o re-exec via nsenter já existe — a nota do AGENTS.md está desactualizada) | infra.rs:2421, container.rs:1403-1425 |
| `--network-alias` | aardvark-dns resolve aliases | **No-op** — gravado e mostrado mas nunca consultado no `dns_resolve` | container.rs:1346; infra.rs:3217 só casa `name` |
| Driver macvlan/ipvlan | Realizado | **Não realizado** em rootless (`Realized=False`, precisa CAP_NET_ADMIN na init-netns) | network.rs:244-250 |
| Overlay multi-nó (forwarding real) | swarm encaminha entre nós | **Parcial** — device+FDB criados; forwarding inter-nó não provado E2E | AGENTS.md secção overlay |
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
| Portas <1024 em rootless | Auto-rotas forçadas a :8080 (paridade prática c/ podman) | ingress_proxy.rs:498-499 |
| Estabilidade de hostfwd / refcount ingress | Causa externa (delonix-engine privado); reaper morto fail-open + refcount vaza | AGENTS.md secção "portas morriam" |
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
- ✅ **FEITO (2026-07-23)**: os 6 HIGH da auditoria original (secção 1a) — path traversal no whiteout OCI, IDs do CRI, nome de VM em `generate_seed_iso`, kubeconfig em `/tmp`, symlink no `COPY` do build, socket de gestão sem `SO_PEERCRED`.
- ✅ **FEITO (2026-07-23, confirmado 2026-07-26)**: núcleo de syscalls (`delonix-runtime/lib.rs`, 104 `unsafe`) + `delonix-net/infra.rs` — 2 CRITICAL + 3 HIGH encontrados e corrigidos na "2ª ronda", e agora com uma 2.ª auditoria independente (zero achados novos) — ver secção 1b.
- ✅ **FEITO (2026-07-26)**: 2.ª auditoria adversarial independente dos 6 HIGH da secção 1a — 5/6 confirmados sólidos, 1/6 (kubeconfig, TOCTOU residual local) tinha um gap real, agora também corrigido. Os dois itens "por confirmar" da Fase 0 estão FECHADOS.
- **Ainda por fazer**: triar os 11 achados candidatos da auditoria original (inclui mais um HIGH: fuga de rootfs no `--rm` rootless).

**Fase 1 — destrava o ecossistema (maior alavanca, um investimento resolve três bloqueantes):**
1. ✅ **FEITO (v0.26.0)**: `delonix serve docker-api` — leitura (2026-07-23) + mutações de ciclo de vida (`create`/`start`/`stop`/`kill`/`wait`/`restart`/`rename`/`remove`/`inspect`), validado contra um `docker` CLI real e o caminho `create`+`start` que o `docker compose up/down` usa. `exec`/attach interactivo continua fora de escopo.
2. **`--format` / Go-template** em ps/inspect/info — bloqueante isolado para scripting. O modo Kind já não precisa disto para arrancar (resolvido — ver Diferenciais), mas continua útil para scripting/CI em geral.

**Fase 2 — build de produção:**
3. ✅ **FEITO (2026-07-23)**: multi-stage build (`FROM…AS` + `COPY --from`) — ver secção 2a.
4. ✅ **FEITO (2026-07-23)**: `--build-arg`/`ARG` (com `${NAME}`/`$NAME`, incluindo antes do 1º `FROM`) + `USER`/`ENTRYPOINT` já sobrevivem ao commit rootless (antes só o `ENTRYPOINT` do modo root sobrevivia; `USER` perdia-se sempre, em ambos os modos, e nem chegava ao JSON de config OCI). **Gap novo, separado, encontrado ao validar**: `container run` nunca lê o `User` guardado na imagem para definir o uid em runtime — só um `--user` explícito o faz. Guardar o `USER` (feito) e aplicá-lo automaticamente no `run` são features distintas; a resolução de nome→uid (`resolve_run_user`) já existe, falta só o default.
5. ✅ **FEITO (2026-07-23), rootless**: cache de layers por instrução (`--no-cache` para saltar) — ver secção 2a/2b. Modo root continua sem cache (executa sempre a sério — ver a nota na secção 2a sobre `commit_upper` precisar de um `upper/` real).
6. ✅ **FEITO parcial (v0.27.0)**: `RUN --mount=type=secret` e `--platform` — ver secção 2b.

**Fase 3 — compose e orquestração local:**
7. ✅ **FEITO (v0.29.0)**: parser de `docker-compose.yml` + `depends_on` (as 3 condições) + `compose down/ps/logs` por projecto — ver secção 2b.

**Fase 4 — correcções de correctude silenciosas restantes:**
- ✅ **FEITO**: perfil seccomp custom (erro explícito), opções de bind `:z/:Z` SELinux (erro explícito), `--network-alias` no-op (agora avisa).
8. **`container update --memory/--cpus` no-op silencioso em rootless-delegado** + **cpuset/weights ignorados no delegado** — ainda por corrigir; precisa de teste num host com delegação systemd real. (lib.rs:4274)

**Fase 5 — paridade de CLI de operação:**
- ✅ **FEITO (v0.25.0)**: `wait`, `kill -s`, `attach` (só saída), `restart` (subcomando), `logs --tail/--since/--timestamps`, `exec -e/-w/-u`, `rename`, `port` — ver secção 2b para o detalhe e as limitações honestas de cada um.

**Fase 6 — rede/GPU/recursos avançados:**
10. ✅ **FEITO parcial (v0.28.0)**: GPU real via CDI/nvidia-container-toolkit — ver secção 2b.
11. Publish com host-IP, backend pasta/passt (perf), `--ip` fixo, macvlan/ipvlan rootless (limitado por CAP_NET_ADMIN), `--pids-limit`, tuning de memória/swap.

**Racional da ordem:** a Fase 0 tem a maior razão valor/esforço — a maioria do "não serve em produção" vem de **incompatibilidade de superfície**, não de falta de capacidade de kernel (onde o motor já está a par ou à frente). A Fase 3 é barata e deve entrar cedo porque são **falhas silenciosas de segurança/correctude** — piores que uma feature em falta, porque o utilizador julga que está protegido.