# Matriz CaaS do delonix-runtime (2026-10-08)

Levantamento read-only, feito a partir do HEAD deste worktree (`1cbe9639`, tag `v5.0.0`),
para preparar o delonix-runtime como base técnica de um **Containers as a Service**
externo. Metodologia: cada afirmação está confirmada no código actual (grep + `Read` dos
ficheiros reais, assinaturas de função, testes existentes) — o `AGENTS.md` serviu só de
mapa; onde ele e o código discordavam (ver nota sobre ACH-034 abaixo da matriz), venceu o
código. Não se correu `cargo build`/`cargo test`/nada privilegiado — é inventário estático.

Convenção de **Estado**:
- **PASS medido** — há teste automatizado ou um caminho de prova ao vivo já escrito no
  próprio repo (`scripts/e2e.sh`, teste unitário que falha com a correcção revertida).
- **PASS por leitura** — o código implementa a capacidade de forma consistente com o que
  afirma, mas não encontrei um teste/`check` dedicado nesta passagem.
- **PARTIAL** — funciona para um subconjunto de casos, com o resto explicitamente recusado
  ou avisado (não é falha silenciosa).
- **GAP** — a capacidade existe mas tem um defeito real, ou falta sem aviso.
- **NOT SUPPORTED** — deliberadamente fora de escopo, com razão escrita no código.

## 1. Matriz de capacidades

| Capacidade | Backend(s) | Implementação | Evidência | Estado | Gap concreto | Teste de aceitação sugerido |
|---|---|---|---|---|---|---|
| **Compatibilidade OCI — Image Spec** (pull/push/manifest/layers) | `delonix-oci` | `crates/adapters/delonix-oci/src/registry.rs::{pull_from_registry_with_creds_full, push_to_registry, verify_manifest_digest}` (linhas ~1781-2300); CAS em `src/cas.rs::has` | Teste `pull_from_registry_with_creds_salta_blobs_ja_no_cas` (registry.rs:3522); retomada de blob cortado testada com correcção revertida | PASS medido | — | já cobre; manter |
| **Compatibilidade OCI — Runtime Spec (bundle `config.json`/`runc create`)** | `delonix-linux` | `crates/adapters/delonix-linux/src/lib.rs` cabeçalho: *"It is Month 5's `mini-runc`, promoted to a library"* — reimplementação nativa que IMITA o comportamento observável do runc (ordem de mounts, dev nodes, `/proc/sys`), não consome nem produz um bundle OCI Runtime Spec | grep `runc\b` em lib.rs (10+ comentários de paridade de comportamento, nenhum `config.json`/`oci-runtime-spec` crate) | PARTIAL | Não há verbo `create <bundle>` compatível com a OCI Runtime CLI — não pode substituir `runc`/ser usado como `containerd-shim-runc-v2`. É CRI-first (servidor gRPC próprio), não bundle-first. Documentar explicitamente esta fronteira para quem vende isto como "OCI compliant" | Correr `oci-runtime-tool validate` contra um bundle gerado por este motor — hoje não há como gerar um |
| **`kind: SystemContainer` nunca entra pelo caminho OCI/`Container`** | `delonix-compute::system_container` + `delonix-proxmox` (LXC) | `crates/contexts/delonix-compute/src/system_container.rs` (trait `SystemContainerProvider`, zero dependência de `RunOpts`/`Container`); CLI em `bins/delonix-runtime-bin/src/cmd/system_container.rs` importa só `delonix_compute::system_container::{...}`, nunca `cmd::container` | `kinds.rs:469-485` — `SYSTEM_CONTAINER` tem `Namespaced::Never` (o `CONTAINER` da linha seguinte não), `stack_group: "systemContainers"` próprio; campos de privilégio (`unprivileged`/`privileged`/`features`/`nesting`) recusados por nome, `manifest.rs:444-447` | PASS medido | Ver secção 2 abaixo | — |
| **Autenticação em registries** | `delonix-oci` | `registry.rs` (Basic/Bearer, token cache em disco, `delonix image login`) | — | PASS por leitura | — | — |
| **Pull by digest com verificação do MANIFESTO** (não só dos blobs) | `delonix-oci` | `registry.rs:1825 verify_manifest_digest` | teste dedicado citado no AGENTS.md (CRÍTICO #3 fechado) | PASS por leitura | — | — |
| **Cache de conteúdo (CAS) com dedup e retomada** | `delonix-oci` | `cas.rs::has`, `blob_with_progress_capped` (retomada por `Range`) | testes de retomada citados (falham com correcção revertida) | PASS medido | — | — |
| **Build de imagens (Delonixfile/Dockerfile, multi-stage, CNB)** | `delonix-oci` + `delonix-compute` | `cmd/build.rs`, `delonix-oci::buildpack.rs` | — | PASS por leitura | Build rootless com muitas layers reais usa a API `fsopen`/`fsconfig` nova (ADR-0037) — confirmado no overlay de EXECUÇÃO; não verifiquei se o `build`'s próprio container de trabalho usa o mesmo caminho | correr `delonix build` com uma imagem base de 90+ layers |
| **Rede: isolamento por namespace lógico (default-deny cross-namespace)** | `delonix-sdn` | `infra.rs:3219 do_attach`, chains `@dlxall`/`@dlxns_<ns>`, `fwcont`/`fwdeny` (linhas 789, 3290, 5287) | testes com `ip saddr @dlxall ... drop` (infra.rs:10050-10225) | PASS medido | A isolação exige `br_netfilter`; sem ele é INERTE — mas desde a decisão D5 (2026-10-02) o motor RECUSA o workload (DX-6305) em vez de deixar passar em silêncio, salvo `DELONIX_ALLOW_UNENFORCED_ISOLATION=1` | — |
| **Anti-spoofing de MAC/IP por porta** | `delonix-sdn` | `table bridge dlxspoof` (infra.rs:3537-3544) | testes com JSON real do `nft -j` (infra.rs:11340-11394) | PASS medido | — | — |
| **DNS interno por serviço/namespace** | `delonix-sdn` | `dns_resolve`/`dns_resolve_multi_for`, `<nome>.<ns>.delonix.internal` | citado no AGENTS como E2E provado | PASS por leitura | — | — |
| **Publish de porta com endereço de bind persistido** | `delonix-sdn` + CLI | `normalize_publish_spec`, `comparable_ports` | gate e2e.sh "publish: o endereço de bind é REGISTADO" (10/10 com fix, 6/10 sem) | PASS medido | `hostPort` SCTP em rootless é recusado pelo nome (ADR-0074) — PARTIAL por desenho, não bug | — |
| **Firewall por container (`net ingress`/`net egress`, `NetworkPolicy`)** | `delonix-sdn` + `cmd/firewall.rs` | chain `fwcont`, verdict map `@fwmap` | contadores `nft` reais citados | PASS por leitura | — | — |
| **Volumes nomeados e bind mounts** | `delonix-volume` | `VolumeStore::resolve_spec` | — | PASS por leitura | — | — |
| **Path traversal em bind mounts/`COPY`** | `delonix-linux` + `delonix-oci` | `safe_bind_target` (lib.rs:1250), `confine_to` (build.rs) | auditoria de segurança #1 cita exploit reproduzido e corrigido | PASS medido | — | — |
| **Posse/ownership de volumes e rootfs (USER da imagem)** | `delonix-linux` + `delonix-compute::run` | `resolve_run` (run.rs:89-144), índice `overlay-owners` (lib.rs:3732+) | `image_user_is_non_root` testado (run.rs:806-807) | PASS por leitura | **Ver Gap crítico #2 abaixo — o fallback para root é inobservável** | — |
| **Secrets — valores nunca em claro no registo/`inspect`** | `delonix-state::SecretStore` + `delonix-compute::record` | `record.rs:396-402` (`secrets: Vec<String>` = NOMES, não valores), `secret.rs` (AEAD at-rest) | teste `value_encrypted_at_rest_and_legacy_plaintext_readable` (secret.rs:533) | PASS medido | — | — |
| **Secrets — `--secret-files` nunca toca o disco do host/container** | `delonix-linux` | `write_secret_files` (lib.rs:2750-2780) | **ZERO testes** (confirmado por grep — só 1 chamador, nenhum `#[test]`) | **GAP** | **Ver Gap crítico #1 abaixo** | montar `/run/secrets` sobre um `tmpfs` que falha de propósito (ex. já montado como RO) e confirmar que NADA é escrito no overlay persistente |
| **Limites de CPU/memória/PIDs aplicados de verdade em rootless delegado** | `delonix-linux` | `cgroup_limits_apply`, `leaf_controllers`, `preflight_controller_limits` (container.rs:2381) | validado ao vivo (citado), `DELONIX_ALLOW_UNENFORCED_LIMITS` fail-closed por omissão | PASS medido | `--cpuset`/`--io-weight`/`--device-*` recusados sem delegação explícita do systemd (não é gap, é recusa correcta) | — |
| **`container update` aplica limites no cgroup REAL (não no caminho estático)** | `delonix-linux` | `update_limits` usa `live_cgroup(container)` (lib.rs:9184-9196) | — | PASS por leitura | — | — |
| **Detecção de OOM-kill real** | `delonix-linux` | `oom_kill_count`/`died_of_oom` (lib.rs:108-145, 6932-7074) | testes `reads_the_oom_kill_counter...` (lib.rs:11407-11450) | PASS medido | Lido só pelo supervisor/`waitpid` no instante da morte — se o cgroup desaparecer antes (ver AGENTS), a detecção falha; mitigado mas documentado como janela estreita | — |
| **Health checks (probe próprio ou do `HEALTHCHECK` da imagem)** | `delonix-compute`/CLI | `apply_probe` (puro, container.rs:6635-6660), `health_monitor_loop` (6845-6889) | testes `apply_probe_...` (8449-8489) | PASS medido | Só monitoriza containers DETACHED (thread do supervisor); um container em foreground não tem health loop — decisão deliberada ("you are looking at it"), não bug | — |
| **Readiness/starting/retries com grace period** | `delonix-compute` | `apply_probe` (regra Docker: sucesso promove de imediato, falha na janela fica `starting`) | testes dedicados | PASS medido | — | — |
| **Sinais e paragem controlada (SIGTERM → espera → SIGKILL)** | `delonix-linux` | `stop_waiting` (lib.rs:8334-8368), `parse_signal`/`cmd_kill` (container.rs) | — | PASS por leitura | `DX-8101`/`StillExiting`: um `stop` que desiste mantém o registo e já libertou portas — o processo pode continuar a existir noutro estado por minutos num disco saturado (documentado, não escondido) | — |
| **Códigos de saída reais, sempre capturáveis para um container detached** | `delonix-compute::launch` + CLI | `should_supervise` (launch.rs:93-97, `detach && forkable`, ignora a policy), `wait_for_exit`/`exit_code_unknown` (container.rs:3549-3551, 4740-4766) | CRI e docker-api re-executam via `__apirun` num processo fresco single-threaded precisamente para tornar `forkable=true` (lifecycle.rs:1513-1517; dockerapi.rs:126,241) | PASS medido | `forkable=false` só no caso teórico de um chamador multi-thread que NÃO re-execute — hoje todos os 3 pontos de entrada (CLI, docker-api, CRI) já re-executam | — |
| **Restart policies (`always`/`unless-stopped`/`on-failure[:N]`)** | `delonix-linux` + `delonix-compute` | `policy_supervised` (launch.rs:87-90), supervisor com `RESTARTS` | citado no AGENTS, `stopped_by_user` impede ressurreição indesejada (container.rs:4628-4634) | PASS por leitura | — | — |
| **Actualização/substituição de workload (hot vs. replace)** | `delonix-stack::reconcile` + `container update` | `hot_fields`/`Action::Replace` (reconcile.rs:119-371) | testes (reconcile.rs:717,929) | PASS por leitura | — | — |
| **Eliminação e limpeza — processo, cgroup, overlay, rede** | `delonix-linux` + `delonix-sdn` | `discard_child` (lib.rs:6162), `remove_container_cgroup` (8923), `reap_orphan_hostfwds` (infra.rs:7807, exige `AuthoritativeLivePorts` — tipo que impede fail-open), `reap_orphan_leases` (ipam.rs:577) | AGENTS documenta medição ao vivo (órfãos 391→47 antes/depois) | PASS por leitura (reaping existe); **PARTIAL** no agendamento | A ceifa de órfãos (IPAM/hostfwd/cgroup) é **só sob pedido** (`system prune`/`network ipam prune`) — nada neste repo agenda-a periodicamente; o AGENTS.md diz que o cron/timer vive no `delonix-deploy` (fora deste repo). Um CaaS construído SÓ sobre o delonix-runtime, sem esse outro repo, não tem GC automático | medir o tamanho de `ipam/*.json`/`containers/` depois de 1000 ciclos run+SIGKILL-do-holder sem nunca correr `system prune` |
| **Rootless vs rootful — limitações explícitas** | `delonix-linux` | `can_map_id_range`, `DELONIX_ALLOW_UNENFORCED_LIMITS`, mensagens dirigidas em vez de falha muda | run.rs:120-144 | PASS por leitura | — | — |

## 2. Containers de sistema vs OCI — a distinção mantém-se

Confirmado directamente no código (não só no AGENTS.md, que documenta a decisão ADR-0058
mas não é prova):

- `crates/contexts/delonix-compute/src/system_container.rs` define um trait próprio,
  `SystemContainerProvider`, com `create/observe/stop/destroy/update_resources/snapshot/...`
  — **nenhuma das suas assinaturas usa `RunOpts`, `Container` ou qualquer tipo do caminho
  OCI**.
- `bins/delonix-runtime-bin/src/cmd/system_container.rs` (a casca CLI/manifesto do Kind)
  importa só `delonix_compute::system_container::{...}` — não importa nada de
  `cmd::container`.
- `crates/contexts/delonix-stack/src/kinds.rs:469-485`: `SYSTEM_CONTAINER` e `CONTAINER`
  são duas entradas distintas em `KindFacts`, com `Namespaced::Never` para o primeiro
  (a isolação do motor não alcança o nó do provider — comentário explícito) contra o
  `Namespaced` do `Container`.
- `bins/delonix-runtime-bin/src/cmd/manifest.rs:444-447` e
  `system_container.rs:394-409`: campos de privilégio (`unprivileged`, `privileged`,
  `features`, `nesting`) são **recusados pelo nome** (DX-1540) no `kind: SystemContainer`
  — nunca silenciosamente ignorados nem encaminhados para o motor de containers.
- O backend real de `SystemContainerProvider` é o `delonix-proxmox` (LXC via API REST de
  um nó Proxmox) — está fisicamente fora do motor de containers (`delonix-linux` nunca é
  chamado para um `SystemContainer`).

**Conclusão**: a confusão que o enunciado pediu para evitar — tratar um LXC do Proxmox
como se fosse um container OCI — **não existe no código actual**. É uma fronteira de tipo
(trait diferente) e de Kind (campo `Namespaced` diferente), não apenas uma convenção de
nomenclatura.

## 3. Daemonless preservado

Processos que precisam de continuar vivos para uma capacidade de CaaS funcionar, e o que
confirma que nenhum é um daemon global residente por omissão:

| Processo | Quando nasce | Vida | Confirmação |
|---|---|---|---|
| **Holder de rede (pin+control)** | Lazy, no primeiro `container run`/`vm create` que precise de SDN | Por nó, sobrevive a um restart do plano de controlo (pin nunca morre; `control` reinicia) | `infra.rs:1449 start_control`, `infra.rs:1591 start_pin` — nenhum dos dois é iniciado no arranque do binário; só por `net netns up`/attach |
| **`slirp4netns`** | Por container/por-ingress que publique porta OU peça rede custom | Até o container morrer (ligado ao ciclo de vida, com reaping de órfãos) | Secção "Slirp não sai com o alvo" do AGENTS, confirmado por `reap_slirp_for` |
| **Supervisor por container detached** | Todo `run -d` (`should_supervise = detach && forkable`) | Morre com o container | `launch.rs:93-97` |
| **Thread de health-check** | Spawnada pelo supervisor, só se `--health-cmd`/`HEALTHCHECK` existir | Morre com o supervisor | `container.rs:6845 health_monitor_loop` |
| **Log shim** | Fork por container com `--log-cri` | Morre com o container | citado em AGENTS, lifecycle de fds fechado (#646-648) |
| **Proxy L7 (`ingress-proxy`)** | Só quando um `HTTPRoute`/`--expose` existe | Persiste como a infra de SDN, mas só se algo o pedir | `cmd/httproute.rs`/`cmd/ingress_proxy.rs`, subcomando OCULTO |
| **`delonix-cri`/`delonix-mgmt`/`delonix-mcp`/`delonix-node-api`** | Opt-in via `serve <x>` ou `install.sh --with-cri` | Resident enquanto o operador decidir correr | `delonix serve cri` faz `exec` de um binário irmão — nunca arranca sozinho |

**Confirmação de que não há daemon global por omissão**: `grep -rn "fn main"
bins/delonix-runtime-bin/src/main.rs` mostra um binário CLI que termina depois de cada
comando; nada no arranque do processo sobe um dos processos acima sem um gatilho explícito
do utilizador (uma `run`, um `serve`, um `net netns up`). Isto confirma o princípio
"daemonless" do AGENTS.md ao nível do código, não só da prosa.

## 4. Os 3 gaps mais graves

### Gap #1 (SEGURANÇA + DADOS DO TENANT) — `--secret-files` pode escrever segredos em claro no disco persistente sem avisar

**Onde**: `crates/adapters/delonix-linux/src/lib.rs:2750-2780`, função `write_secret_files`.

**O que o comentário promete**: *"the values stay only in RAM (tmpfs) — they never touch
the host fs nor the container's, nor the environment."*

**O que o código faz**: a função cria `/run/secrets` (`create_dir_all`, erro descartado com
`if ...is_err() { return; }`), tenta montar um `tmpfs` ali (`let _ = mount(...)` — **o
resultado do `mount()` é descartado por completo**, sem verificação), e **só depois**
escreve os pares `(chave, valor)` para dentro de `/run/secrets/<chave>` com
`std::fs::write`. Se o `mount()` do tmpfs falhar por qualquer razão (ordem de montagem,
uma flag rejeitada por um kernel específico, `/run/secrets` já ocupado por outra coisa, um
cgroup/userns mais restrito do que o esperado), o código **não detecta a falha** e continua
a escrever os valores — que caem então no `/run/secrets` do **rootfs real do container**
(a camada de escrita do overlay, persistida em disco no host) em vez de RAM. Zero teste
cobre este caminho (confirmado: `grep -n write_secret_files` só devolve a definição e o
único chamador, sem `#[test]` nenhum).

**Risco**: um segredo de um tenant (password de base de dados, chave de API) fica escrito
em claro na camada persistente do container — sobrevive a um `container commit`
(potencialmente entra numa imagem publicada) e sobrevive num backup/snapshot do volume de
sistema de ficheiros, exactamente o cenário que `--secret-files` existe para evitar face a
`--secret` (env vars).

**Teste de aceitação**: montar `/run/secrets` como só-leitura ANTES do spawn (ou injectar
uma falha determinística no `mount()`, ex. via um mock/feature de teste) e confirmar que
`write_secret_files` **recusa** (retorna erro, propagado até `cmd_run` que aborta o `run`)
em vez de degradar para escrita no disco persistente. Hoje a função é `fn
write_secret_files(pairs: &[...])` sem tipo de retorno — a correcção minimamente invasiva é
mudar a assinatura para `-> Result<()>`, verificar o `mount()` e propagar o erro ao
chamador em `container_init`.

### Gap #2 (SEGURANÇA + MENTE SOBRE O ESTADO REAL) — o fallback silencioso para root (ADR-0062) é inobservável por qualquer consumidor externo

**Onde**: `crates/contexts/delonix-compute/src/run.rs:120-144` (lógica do fallback);
`bins/delonix-runtime-bin/src/cmd/container.rs:1029-1038` (`print_notices`, só
`eprintln!`); `crates/interfaces/delonix-cri/src/runtime_svc/lifecycle.rs:508-520`
(`delonix_detached_why_in` — stderr capturado num ficheiro temporário e **descartado sem
leitura** quando `status.success()`).

**O que acontece**: desde o ADR-0062 (major breaking change aceite), um container sem
`--user` explícito corre por omissão com o `USER` que a IMAGEM declara. Num host sem
intervalo de subuid/subgid (`host.can_map_id_range() == false` — rootless single-uid, ou
qualquer sessão sem `/etc/subuid` configurado), o motor **não falha** — cai de volta para
uid 0 (root) dentro do container e emite um `Notice` de aviso. Esse `Notice`:
1. Nunca é persistido em `crates/contexts/delonix-compute/src/record.rs` — **não há campo
   `run_user`/`effective_uid` no `Container` record** (confirmado por grep: `run_user` só
   existe na struct transitória `ResolvedRun`, nunca chega ao `Container` guardado em
   disco).
2. É impresso só via `eprintln!` na CLI (`print_notices`), e num `run -d`/CRI a saída
   padrão/erro do processo que o criou **não é o que fica acessível depois** — é descartada.
3. No caminho CRI especificamente: o `StartContainer` chama
   `delonix_detached_why_in(base, netns, &["__apirun", &spec_arg])`, que grava o stderr
   do `__apirun` (onde o Notice seria impresso) num ficheiro temporário — mas **só lê esse
   ficheiro quando o processo falha** (`if status.success() { return Ok(None); }`, linha
   519-520, sem nunca inspeccionar `stderr` nesse ramo). O ficheiro é apagado a seguir
   (linha 517, antes mesmo deste `if`).

**Resultado**: um orquestrador CaaS que cria containers via CRI (o caminho natural para um
kubelet) **não tem forma nenhuma** de descobrir, depois do facto, que um Pod/container que
declarou um `USER` não-root está de facto a correr como root — nem via
`ContainerStatus`/`inspect`, nem via logs, nem via o journal de eventos
(`delonix_node::events::emit` não é chamado neste caminho). É exactamente a classe
"comportamento que mente sobre o estado real" que o enunciado pede para caçar: o motor SABE
que desviou da intenção declarada e NÃO guarda esse facto em lado nenhum consultável.

**Teste de aceitação**: criar um container (via CRI, para validar o caminho mais relevante
para CaaS) com uma imagem cujo `USER` é não-root, num ambiente de teste onde
`can_map_id_range()` é forçado a `false`; depois do `start_container` ter sucesso, chamar
`ContainerStatus`/`container inspect` e exigir um campo explícito (`effective_uid: 0`
+ uma flag `user_fallback: true`) — hoje nenhum dos dois existe. Como correcção minimamente
invasiva: persistir `run_user`/o facto do fallback no `Container` record (ADR-0062 já tem a
informação calculada em `resolve_run`, só falta transportá-la), e emitir
`delonix_node::events::emit` com esse detalhe para o caminho CRI aparecer no journal mesmo
quando a chamada tem sucesso.

### Gap #3 (SEGURANÇA — ADMISSÃO MULTI-TENANT DESLIGADA POR OMISSÃO) — "sem política = sem tecto" e "sem tecto de capacidades = ilimitado" são os DOIS defaults

**Onde**: `bins/delonix-runtime-bin/src/cmd/policy.rs:182-189` (doc-comment explícito: *"No
policy = no ceiling"*, `enforce()` devolve `Ok(())` de imediato quando `load(root)?` é
`None`); `crates/interfaces/delonix-cri/src/cap_ceiling.rs:84-94,127-132` (`CeilingMode::parse("")
= Reject` mas o **spec** vazio dá `mask: None` = sem tecto nenhum de capacidades — um pedido
`privileged: true` via CRI recebe TODAS as capabilities do kernel salvo o operador do nó ter
configurado explicitamente `DELONIX_CRI_CAP_CEILING`/`--cap-ceiling`).

**Porque é grave especificamente para CaaS**: um motor de nó único (que é o que este
repositório é, por desenho — "o motor não conhece nenhum consumidor") assume
correctamente que o OPERADOR DO NÓ decide os limites. Mas um CaaS aceita `PodSpec`s/manifestos
de TENANTS não confiáveis e encaminha-os, tipicamente sem tradução, para o CRI ou para a CLI.
Sem que a camada CaaS acima deste motor configure explicitamente as DUAS coisas — um
`kind: RuntimePolicy` aplicado E `DELONIX_CRI_CAP_CEILING` definido — **qualquer tenant pode
pedir `privileged: true` e obter compromisso total do nó**, exactamente como um operador
root teria. Isto não é um bug (está documentado e é intencional para o caso de uso
single-tenant do motor), mas é o item de configuração que, se esquecido, dá a um CaaS
inteiro uma fronteira de confiança inexistente — e o motor não oferece um modo
"secure-by-default" alternativo (ex.: recusar `privileged`/capabilities extra salvo
allowlist, por omissão, num nó sem política nenhuma carregada).

**Teste de aceitação**: num nó limpo, SEM `kind: RuntimePolicy` aplicado e SEM
`DELONIX_CRI_CAP_CEILING` definido, criar um container/Pod com `privileged: true`
(CRI) ou `--privileged` (CLI) e confirmar que é **aceite** (hoje é — isto é o
comportamento actual, não hipotético). Para um CaaS, a recomendação é: a camada de
integração nunca expõe o motor sem primeiro aplicar ambos; o próprio motor poderia, no
mínimo, emitir um AVISO estrutural (evento, não só texto) sempre que um pedido privilegiado
é aceite SEM política nenhuma carregada — hoje não emite nenhum.

---

## Notas metodológicas (para quem continuar este levantamento)

- **ACH-034 (`secret rotate-key`) — o AGENTS.md contradiz-se a si próprio, e o código
  resolve a contradição**: uma secção do AGENTS.md ("ADR-0069 item 6") diz que o bug da
  versão a recuar para 1 numa rotação de chave foi corrigido; outra secção, mais abaixo
  ("A bateria mede o `--help` de tudo"), di-lo como `xfail` ainda activo. `git log` confirma
  que o COMMIT da correcção (`bbf2a972`, 15:50) é posterior ao commit que introduziu o
  `xfail` no `scripts/e2e.sh` (`d684526b`, 14:41), **no mesmo dia** — ambos são ancestrais do
  HEAD actual. Ou seja: **o bug está corrigido no código** (`SecretStore::write` com
  `keep_version`, chamado por `rotate_key`), mas o marcador `xfail ACH-034` no
  `scripts/e2e.sh` ficou esquecido e devia ter saído por XPASS. Não é um gap funcional —
  é uma dívida de housekeeping no próprio script de bateria, que valeria a pena limpar
  numa sessão de manutenção (fora do âmbito desta investigação CaaS).
- Não corri `cargo build`/`cargo test`/`scripts/e2e.sh` — toda a "evidência" citada como
  "PASS medido" refere-se a testes/checks JÁ ESCRITOS no repositório (confirmados por
  leitura), não a uma execução feita nesta sessão.
