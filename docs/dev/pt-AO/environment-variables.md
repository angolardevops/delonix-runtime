<!-- translated-from: environment-variables.md sha256:15aedcb799916846f25f3495c9deb547007ed75b76f5f8342e6620b23de42fea -->
# Variáveis de ambiente (`DELONIX_*`)

**Antes de leres:** [Isolar o estado do motor](build-and-test.md#isolating-the-engines-state) em Clonar, construir e testar.

Esta página lista todos os nomes `DELONIX_*` que aparecem no código do motor, com o sítio onde são
lidos, o que mudam e se alguma vez os deves definir. É uma referência: lê a secção que corresponde
ao que estás a fazer, não a página inteira. Depois dela consegues isolar uma corrida, ligar os
diagnósticos de que precisas, e reconhecer as variáveis que baixam uma fronteira de segurança
antes de definires uma.

## Como ler esta página

Cada linha das tabelas de variáveis tem cinco colunas:

- **Lida por** — o crate ou binário e o `path:symbol` onde o valor é lido (ou escrito).
  Os caminhos são relativos à raiz do repositório.
- **Finalidade** — o que a variável muda.
- **Valores / omissão** — como o código a interpreta. Quando o código recorre em silêncio a outro
  valor por não conseguir interpretar o dado, a linha di-lo.
- **Notas** — quando a usar, e qualquer aviso.

A maioria das variáveis é lida pelo processo que precisa delas e **não** é passada adiante
automaticamente. Algumas são definidas pelo próprio motor nos processos que arranca; essas estão em
[Definidas pelo próprio motor](#set-by-the-engine-itself-internal) e não as deves definir.

**Esta tabela é verificada pela CI nos dois sentidos.** `python3 scripts/dev_docs.py --check`
(função `env_var_problems` em `scripts/dev_docs.py`) extrai todos os nomes `DELONIX_*` dos literais
de string Rust em `crates/` e `bins/` (incluindo `build.rs` e `env!()`) e do `scripts/install.sh`,
e falha quando falta aqui um nome ou quando esta página lista um nome que o código já não contém.
Uma linha só conta quando começa por `` | `DELONIX_NAME` ``. A extracção é textual, por isso apanha
também alguns nomes que **não** são variáveis de ambiente (uma constante Rust, fixtures de teste,
uma chave escrita numa imagem de VM); esses estão listados em secções próprias para que a
verificação continue exacta.

As variáveis usadas apenas por scripts fora desse âmbito — por exemplo `DELONIX_CHAOS_DIR` e a
família `DELONIX_CHAOS_TRUENAS_*` em `scripts/chaos.sh` — estão documentadas no cabeçalho de cada
script e no [Clonar, compilar e testar](build-and-test.md), não aqui.

### Precedência

Onde o código tem uma regra de precedência, ela é normalmente **flag > ambiente > omissão**; a tabela mostra cada caso tal como o código o resolve, incluindo os que têm um nível a mais ou nenhuma flag:

| Definição | Ordem (a primeira ganha) | Onde |
|---|---|---|
| Socket do CRI, tecto de capabilities e modo | `--addr` / `--cap-ceiling` / `--cap-ceiling-mode` > `DELONIX_CRI_ADDR` / `DELONIX_CRI_CAP_CEILING` / `DELONIX_CRI_CAP_CEILING_MODE` > `unix:///run/delonix-cri.sock` / sem tecto / `reject` | `crates/interfaces/delonix-cri/src/bin/delonix-cri.rs:main` |
| Socket da API de gestão | `--addr` > `DELONIX_API_ADDR` > `unix:///run/delonix-mgmt.sock` | `bins/delonix-mgmt-bin/src/main.rs:run` |
| Socket da API Docker | `--addr` > `DELONIX_DOCKER_ADDR` > `unix:///run/delonix-docker.sock` | `bins/delonix-runtime-bin/src/cmd/dockerapi.rs:run` |
| Língua da saída | `--l18n` > `DELONIX_L18N` > inglês | `bins/delonix-runtime-bin/src/cmd/po.rs:peek_lang` |
| Tecla de escape da consola de VM | `--escape` > `DELONIX_CONSOLE_ESCAPE` > `^]` | `bins/delonix-runtime-bin/src/cmd/vm.rs:resolve_escape` |
| Backend de VM | `--backend` (ou o `HYPERVISOR` da imagem) > `DELONIX_VM_BACKEND` > `vm default-backend --set` > auto-detecção | `crates/adapters/delonix-vm/src/lib.rs:standing_backend_choice` |
| Endereço de bind das portas publicadas | o endereço em `-p <ip>:<host>:<container>` > `DELONIX_PUBLISH_ADDR` > `127.0.0.1` | `crates/adapters/delonix-sdn/src/lib.rs:publish_bind_addr` |
| Feed de CVE para `image scan --update` | `--feed` > `DELONIX_ADVISORY_FEED` > erro | `bins/delonix-runtime-bin/src/cmd/scan.rs:cmd_scan_update` |
| Filtro de logs | `DELONIX_LOG` > `RUST_LOG` > `info` | `crates/adapters/delonix-telemetry/src/telemetry.rs:init` |

`delonix serve cri` e `delonix serve api` passam as suas flags ao binário de servidor que executam
com `exec` **das duas formas**, como flags e como a variável correspondente
(`bins/delonix-runtime-bin/src/cmd/serve.rs:run`), para que um servidor de uma release anterior que
só lê as variáveis receba mesmo assim o valor.

### Isolar uma corrida de desenvolvimento

Antes de correres qualquer coisa além de `--help` numa máquina que também corre workloads Delonix,
aponta as duas localizações de estado para um directório de rascunho:

```bash
export DELONIX_ROOT=$HOME/scratch/dlx/root        # records, images, networks, IPAM, volumes, pidfiles
export DELONIX_NET_RUNTIME_DIR=/tmp/dlx-run        # the network holder's control and slirp sockets
mkdir -p "$DELONIX_ROOT" "$DELONIX_NET_RUNTIME_DIR"
./target/debug/delonix system info                # "state root" must show your scratch path
```

**Porquê as duas.** A infra-estrutura de rede guarda os seus pidfiles em `DELONIX_ROOT`, mas os seus
sockets unix num directório de runtime separado (`crates/adapters/delonix-sdn/src/infra.rs:runtime_dir`),
porque o caminho de um socket está limitado a cerca de 108 bytes e o `DELONIX_ROOT` pode ser
arbitrariamente profundo. Definir só uma das duas pode deixar dois state roots a partilhar o mesmo
conjunto de sockets. A história completa, e como desmontar a infra isolada, está em
[Isolar o estado do motor](build-and-test.md#isolating-the-engines-state).

## Configuração do dia-a-dia

| Variável | Lida por | Finalidade | Valores / omissão | Notas |
|---|---|---|---|---|
| `DELONIX_PERF_CONF` | `scripts/install.sh` (o `/usr/local/sbin/delonix-performance` gerado) | Caminho da configuração (flags `CPU=`/`THP=`) que o helper lê. | Um caminho de ficheiro. Por definir: `/etc/delonix/performance.conf`. | Só para testar o helper contra um sysfs falso; o `install.sh` escreve o verdadeiro. |
| `DELONIX_PERF_CPU` | `scripts/install.sh` (o `/usr/local/sbin/delonix-performance` gerado) | Raiz da árvore sysfs do CPU que o helper lê e escreve (governor, EPP). | Um caminho de directório. Por definir: `/sys/devices/system/cpu`. | Aponta-a para uma árvore falsa para testar `apply`/`revert` sem tocar no host. |
| `DELONIX_PERF_STATE` | `scripts/install.sh` (o `/usr/local/sbin/delonix-performance` gerado) | Directório onde o helper guarda os valores do arranque que repõe no `revert`. | Um caminho de directório. Por definir: `/var/lib/delonix-performance`. | Substituição só para testes, como as outras. |
| `DELONIX_PERF_THP` | `scripts/install.sh` (o `/usr/local/sbin/delonix-performance` gerado) | O ficheiro `enabled` das hugepages transparentes que o helper lê e escreve. | Um caminho de ficheiro. Por definir: `/sys/kernel/mm/transparent_hugepage/enabled`. | Substituição só para testes, como as outras. |
| `DELONIX_ROOT` | todos os stores e binários: `bins/delonix-runtime-bin/src/cmd/util.rs:state_root`, `crates/adapters/delonix-oci/src/image.rs:ImageStore::default_root`, `crates/adapters/delonix-state/src/store.rs:Store::default_root`, `crates/adapters/delonix-sdn/src/infra.rs:base_root`, `crates/interfaces/delonix-cri/src/bin/delonix-cri.rs:main`, `bins/delonix-mgmt-bin/src/main.rs:run`, `crates/interfaces/delonix-mcp/src/lib.rs` | O state root do motor: registos de containers, imagens, redes, IPAM, volumes, VMs, segredos. | Um caminho de directório. Sem valor: `$XDG_DATA_HOME/delonix` (ou `~/.local/share/delonix`) quando não é root, `/var/lib/delonix` como root. **`delonix-cri` e `delonix-mgmt` usam `/var/lib/delonix` por omissão em qualquer caso**, e é por isso que `delonix serve …` passa explicitamente o root da CLI (`cmd/serve.rs:exec_server`). | A variável a definir quando testas. O motor também a **define** em cada filho que arranca (passagens de re-exec, o holder de rede, servidores, chamadas de ciclo de vida do CRI) para que os caminhos concordem entre user namespaces. |
| `DELONIX_NET_RUNTIME_DIR` | `crates/adapters/delonix-sdn/src/infra.rs:runtime_dir` (`RUNTIME_DIR_ENV`) | Directório dos sockets unix da infra-estrutura de rede (`control.sock`, `slirp.sock`). | Um caminho de directório; mantém-no curto (caminhos de socket acima de ~108 bytes falham com `SUN_LEN`). Sem valor: `/tmp/delonix-net-<uid>` mais um sufixo derivado de um `DELONIX_ROOT` que não seja o de omissão. | Define-a junto com `DELONIX_ROOT` quando isolas. O motor também a passa ao holder e às passagens de re-exec de `--net <custom>` (`infra::runtime_dir_env`), porque dentro do user namespace do holder o uid é 0 e o valor de omissão seria outro. |
| `DELONIX_L18N` | `bins/delonix-runtime-bin/src/cmd/po.rs:peek_lang` | Língua da saída e do `--help` da CLI. | `en` (omissão) ou `pt`. | `--l18n` ganha. As *classes* de erro (códigos de saída) não dependem da língua, as mensagens sim — não faças grep a mensagens em scripts. |
| `DELONIX_LOG` | `crates/adapters/delonix-telemetry/src/telemetry.rs:init` | Filtro de logs para `delonix`, `delonix-cri`, `delonix-mgmt` e `delonix-mcp`. | Uma expressão de filtro do `tracing` (`debug`, `warn`, `delonix_sdn=debug`). Recorre a `RUST_LOG` e depois a `info`. | Os logs vão para o stderr; o stdout fica reservado à saída dos comandos. |
| `DELONIX_LOG_FORMAT` | `crates/adapters/delonix-telemetry/src/telemetry.rs:init` | Formato das linhas de log. | `json` para linhas JSON; qualquer outra coisa (ou sem valor) é texto simples. | Útil quando um servidor corre sob systemd e o seu journal é enviado para outro sítio. |
| `DELONIX_VERBOSE` | `bins/delonix-runtime-bin/src/cmd/output.rs` (`Progress`) | Mostra a saída de cada passo em vez de a condensar numa linha de progresso. | Definida e diferente de `0` → verboso. | O mesmo efeito que `--verbose` onde um comando o tem. |
| `DELONIX_HOSTS_FILE` | `bins/delonix-runtime-bin/src/cmd/hosts_file.rs:hosts_path` | O ficheiro de hosts onde `hosts: [host]` num `HTTPRoute` e o `delonix hosts sync` escrevem o seu bloco gerido. | Um caminho de ficheiro; omissão `/etc/hosts`. | Aponta-o para um ficheiro de rascunho numa corrida isolada, para o `/etc/hosts` verdadeiro nunca ser tocado (escrevê-lo precisa de root). Como o bloco funciona: [Como os nomes chegam ao `/etc/hosts`](service-names-and-hosts.md). |
| `DELONIX_CONSOLE_ESCAPE` | `bins/delonix-runtime-bin/src/cmd/vm.rs:resolve_escape` | A tecla que desliga do `delonix vm console`. | Uma tecla de controlo como `^X` ou `X`. Omissão `^]`. Um valor inválido é um erro, não um recurso a outro valor. | Para layouts de teclado onde `^]` não se consegue escrever (por exemplo o português). `-e/--escape` ganha. |
| `DELONIX_CRI_ADDR` | `crates/interfaces/delonix-cri/src/bin/delonix-cri.rs:main`; passada adiante por `bins/delonix-runtime-bin/src/cmd/serve.rs:run` | Socket onde o servidor CRI escuta. | `unix://<path>`. Omissão `unix:///run/delonix-cri.sock`. | `--addr` ganha. O `--container-runtime-endpoint` do kubelet tem de coincidir. |
| `DELONIX_CRI_CAP_CEILING` | `crates/interfaces/delonix-cri/src/bin/delonix-cri.rs:main` (`cap_ceiling::CEILING_ENV`, interpretada por `crates/interfaces/delonix-cri/src/cap_ceiling.rs:CapCeiling::parse`); passada adiante por `cmd/serve.rs:run` | Limite máximo, ao nível do nó, das capabilities de qualquer container criado através do CRI, `privileged: true` incluído. | Vazia/sem valor ou `all` → sem tecto (comportamento inalterado). `none` → nenhuma capability. `default` → o conjunto de omissão do motor. `default,NET_ADMIN,…` → o de omissão mais as nomeadas. Uma lista de nomes (prefixo `CAP_` opcional, sem distinção de maiúsculas; separados por vírgulas, espaços ou `;`) → exactamente essas. `all` em qualquer ponto da lista ganha. Um nome desconhecido, ou um valor só com separadores, **impede o servidor de arrancar**. | `--cap-ceiling` ganha. Limita só capabilities: um pod privilegiado continua a ter seccomp sem confinamento e um `/sys` com escrita. O tecto em vigor é visível no `crictl info` (`capabilityCeiling`). |
| `DELONIX_CRI_CAP_CEILING_MODE` | `crates/interfaces/delonix-cri/src/cap_ceiling.rs:CeilingMode::parse` (`MODE_ENV`); passada adiante por `cmd/serve.rs:run` | O que acontece quando um pod pede explicitamente mais do que o tecto. | `reject` (omissão; também `enforce` ou vazio) → o `CreateContainer` falha a nomear as capabilities recusadas. `clamp` (ou `trim`) → reduzido ao tecto com um aviso. Uma palavra desconhecida **impede o servidor de arrancar**. | `--cap-ceiling-mode` ganha. Nos dois modos, o conjunto de omissão implícito do motor é reduzido ao tecto sem erro. |
| `DELONIX_CRI_FROM_SOURCE` | `bins/delonix-runtime-bin/src/cmd/vmimage.rs:locate_cri_bin` (`CRI_FROM_SOURCE_ENV`, lida por `cri_from_source_requested`) | Opt-in para compilar o `delonix-cri` a partir do checkout do código-fonte à volta do cwd quando o `cluster apply` / `cluster kubeadm` precisam de um binário para instalar nos nós. | Só `1` ou `true` (depois de aparar) o liga; por definir, vazio, `0` ou qualquer outra coisa é desligado. | Desligado por omissão para o runtime instalado num cluster nunca depender do directório de onde o comando correu. Sem ele a ordem é `--cri-bin`, o `delonix-cri` ao lado do `delonix`, e depois o asset de release da versão em execução, verificado contra o seu `SHA256SUMS`. A origem, o caminho e o sha256 são sempre impressos. |
| `DELONIX_API_ADDR` | `bins/delonix-mgmt-bin/src/main.rs:run`; passada adiante por `cmd/serve.rs:run` | Socket da API de gestão local (`delonix serve api`). | `unix://<path>`. Omissão `unix:///run/delonix-mgmt.sock`. | `--addr` ganha. A API é só local (o uid que chama). |
| `DELONIX_DOCKER_ADDR` | `bins/delonix-runtime-bin/src/cmd/dockerapi.rs:run` | Socket da fatia da Docker Engine API (`delonix serve docker-api`). | `unix://<path>` (o prefixo `unix://` é opcional). Omissão `unix:///run/delonix-docker.sock`. | `--addr` ganha. |
| `DELONIX_VM_BACKEND` | `crates/adapters/delonix-vm/src/lib.rs:standing_backend_choice` | Backend de VM para toda a sessão quando um comando não nomeia nenhum. | Um nome de backend (`libvirt`, `cloud-hypervisor`, ou um backend remoto registado como `proxmox`). Um valor em branco é ignorado. | Abaixo de `--backend` e do `HYPERVISOR` da imagem, acima da omissão da máquina definida com `delonix vm default-backend --set`. Tal como uma escolha explícita, sobrepõe-se à heurística de capacidade e pode falhar tarde, no arranque, se o backend não conseguir correr a VM. |
| `DELONIX_NO_CGROUP_WARN` | `crates/adapters/delonix-linux/src/lib.rs` (o aviso de rootless sem delegação e `warn_if_unprotected_memory`), `bins/delonix-runtime-bin/src/cmd/kindmode.rs` | Silencia os avisos sobre a falta de delegação de cgroup e sobre um container sem tecto de memória em lado nenhum. | Definida (qualquer valor) → silêncio. | O motor **define-a** ele próprio (`cmd/util.rs:silence_cgroup_warning`, `cmd/kindmode.rs`) para que os filhos de re-exec não repitam um aviso que o pai já imprimiu. Defini-la à mão esconde uma condição real: os limites não são aplicados. |
| `DELONIX_POLICY_LINT` | `bins/delonix-runtime-bin/src/cmd/policy.rs:show_lints` | Silencia os avisos de política de runtime emitidos uma vez por comando (`warning: runtime policy [...]`). | `0` → silêncio; qualquer outra coisa ou sem valor → mostrados. | Para quem leu o aviso e decidiu de outra forma. |
| `DELONIX_NO_AUTO_RECOVER` | `bins/delonix-runtime-bin/src/cmd/netns.rs:reconcile_after_respawn` | Depois de o holder de rede ser reconstruído, reporta os containers encalhados e o comando para os reiniciar, em vez de os reiniciar automaticamente. | Definida (qualquer valor) → só reporta. | Para hosts onde queres escolher quando uma base de dados reinicia. |
| `DELONIX_NO_AUTO_DELEGATE` | `bins/delonix-runtime-bin/src/cmd/kindmode.rs` (verificação prévia de cgroup do `cluster create`) | Desliga o re-exec automático do `cluster create` sob `systemd-run --user --scope -p Delegate=yes` quando o controlador `cpu` não está delegado. | Definida (qualquer valor) → imprime o erro em vez de re-executar. | Para scripts e CI que preferem o erro simples. |

## Escapatórias de rede e de segurança

**Todas as variáveis desta secção baixam uma fronteira.** Cada uma regista um `SECURITY WARNING` (ou
um aviso) quando produz efeito. Existem para depuração e para opções de saída explícitas e
informadas; nenhuma pertence a uma configuração de produção. Lê a secção do `AGENTS.md` indicada
antes de usares uma delas.

| Variável | Lida por | Finalidade | Valores / omissão | Notas |
|---|---|---|---|---|
| `DELONIX_FORWARD_POLICY` | `crates/adapters/delonix-sdn/src/infra.rs` (construtor do ruleset de ingress) | Reverte a chain `forward` da netns do holder de negação por omissão (`policy drop`) para permissão por omissão. | `accept` → permissão por omissão; qualquer outra coisa → negação por omissão. | **Baixa o isolamento entre redes.** Registada como aviso de segurança. Ver `AGENTS.md`, *«Bloco 0 do plano 33 (v0.37.1)»*. |
| `DELONIX_ALLOW_LINK_LOCAL` | `crates/adapters/delonix-sdn/src/infra.rs` (chain `fwguard`) | Remove o drop incondicional de `169.254.0.0/16` (metadados de cloud) e `127.0.0.0/8` (loopback do host) para o tráfego dos containers. | `1` → permitido; qualquer outra coisa → descartado. | **Expõe as credenciais de metadados da instância num host de cloud.** Aviso de segurança. `AGENTS.md`, *«Bloco 0 do plano 33 (v0.37.1)»* (RF-NET-02). |
| `DELONIX_ALLOW_HOLDER_INGRESS` | `crates/adapters/delonix-sdn/src/infra.rs` (chain `dlxinput`) | Deixa os containers alcançar os serviços que o próprio holder expõe (o proxy L7, o DNS interno) para além da allowlist. | `1` → permitido; qualquer outra coisa → ligações novas descartadas. | **Através do proxy, um container alcança qualquer backend registado, em qualquer namespace, passando por cima da política de ingress desse backend.** Aviso de segurança. Fundamentação em `docs/discovery/46_GAPS_ENCONTRADOS.md` §4.2. |
| `DELONIX_ENABLE_IPV6` | `crates/adapters/delonix-sdn/src/infra.rs:ipv6_sdn_enabled` | Volta a dar aos containers endereços IPv6 na SDN. | `1` → ligado; qualquer outra coisa → IPv6 desligado no container e forwarding recusado. | **Nenhuma regra de firewall, isolamento de namespace ou `Dependency` se aplica ao IPv6** — todas as políticas são só IPv4. Aviso de segurança. `AGENTS.md`, *«Bloco 0 do plano 33 (v0.37.1)»*. |
| `DELONIX_ALLOW_UNENFORCED_LIMITS` | `bins/delonix-runtime-bin/src/cmd/container.rs:preflight_resource_limits` | Corre um container com `-m`/`--cpus`/`--cpu-weight` mesmo quando esta sessão não tem delegação de cgroup, em vez de recusar (saída 69). | Definida (qualquer valor) → corre, sem limites, com um aviso. | O kernel nunca vê os limites. Ver [delegação de cgroup](environment.md#cgroup-delegation-some-limits-are-refused-others-are-not-enforced). |
| `DELONIX_INSECURE_BESTEFFORT` | `crates/adapters/delonix-linux/src/lib.rs:insecure_besteffort` | Salta a verificação fail-closed do confinamento (seccomp, capabilities, `no_new_privs`) que corre antes do `execve` no init de um container e no `exec`. | Definida (qualquer valor) → a verificação é saltada. | **Um container pode arrancar com um confinamento que, em silêncio, não foi aplicado.** Lida pelo motor antes de o ambiente do container ser aplicado, por isso um container não a consegue definir para si próprio. |
| `DELONIX_PUBLISH_ADDR` | `crates/adapters/delonix-sdn/src/lib.rs:publish_bind_addr`; sugerida por `bins/delonix-runtime-bin/src/cmd/vm.rs` (`vm reach`) | Endereço do host a que as portas publicadas fazem bind quando o `-p` não nomeia nenhum. | Um endereço IPv4; um valor que não seja IPv4 é ignorado. Omissão `127.0.0.1`. | `0.0.0.0` expõe as portas publicadas em todas as interfaces. O `vm reach` sugere o gateway do libvirt para que as VMs consigam alcançar um container sem o expor à LAN. `AGENTS.md`, *«Revisão do flow `-p` ↔ `ingress`/`egress` (2026-07-27)»*. |
| `DELONIX_SUBNET_BASE` | `crates/adapters/delonix-sdn/src/lib.rs:default_base` | Força o segundo octeto da rede de omissão (`10.<base>.0.0/16`). | Um inteiro 0–255 (sem mais verificação de intervalo). Sem valor: o valor persistido em `<root>/net/default-base`, senão um octeto livre detectado no host. | Só para uma colisão que a detecção não viu. Mudá-la depois de existirem containers muda os endereços da rede de omissão. |
| `DELONIX_CNI` | `crates/adapters/delonix-sdn/src/cni.rs:enabled_conf`, consumida por `crates/interfaces/delonix-cri/src/runtime_svc/lifecycle.rs` (`RunPodSandbox`) e `runtime_svc.rs` (`UpdateRuntimeConfig`) | **Só no CRI rootless:** liga à rede os sandboxes de pod através da cadeia de plugins CNI (`/etc/cni/net.d`, plugins do `CNI_PATH`), corrida dentro do holder de rede, em vez da SDN nativa. | `1` → ligado, e só se existir uma configuração; qualquer outra coisa → SDN nativa. | Não tem efeito quando o CRI corre como root: em root usa-se sempre a configuração CNI do nó. |
| `DELONIX_TRACE_UNPUBLISH` | `crates/adapters/delonix-sdn/src/infra.rs:trace_unpublish` | Regista cada despublicação de porta com a função, a porta, o pid, o pid do pai, o executável e um backtrace. | `1` ou `stderr` → stderr; qualquer outro valor → acrescentado a esse ficheiro. | Uma ferramenta de diagnóstico, custo zero quando não definida. Mantida para a investigação em `AGENTS.md`, *«RESOLVIDO — as portas publicadas morriam sozinhas»*. |

## Build e imagens

| Variável | Lida por | Finalidade | Valores / omissão | Notas |
|---|---|---|---|---|
| `DELONIX_INSECURE_REGISTRIES` | `crates/adapters/delonix-oci/src/registry.rs` | Registos contactados por HTTP simples em vez de HTTPS. | Entradas `host` ou `host:port` separadas por vírgulas (sem distinção de maiúsculas). Sem valor → HTTPS em todo o lado excepto nos registos de loopback, que já usam HTTP. | **O tráfego e as credenciais para esses hosts viajam sem TLS.** Uma opção explícita por host, nunca um intervalo. Quando uma ligação HTTPS a um registo falha, o erro sugere esta variável já com o host preenchido. |
| `DELONIX_SCAN_ON_PULL` | `bins/delonix-runtime-bin/src/cmd/scan.rs:admission_scan_on_pull` | Política de admissão de CVE aplicada depois de cada pull de imagem. | Sem valor/vazia → desligada. `warn` → faz o scan e reporta. `low`/`medium`/`high`/`critical` → remove a imagem e recusa quando é encontrada uma vulnerabilidade de pelo menos essa severidade. Um valor desconhecido recusa o pull. | Um gate fail-closed: um erro de escrita não o desliga. Uma imagem sem SBOM é admitida com um aviso. |
| `DELONIX_ADVISORIES` | `bins/delonix-runtime-bin/src/cmd/scan.rs:load_advisories` | Caminho de um ficheiro de base de dados de advisories usado pelo `image scan`. | Um caminho de ficheiro. | Usada só quando não existe uma base de dados sincronizada em `<root>/advisories.json`; a sincronizada ganha. Sem nenhuma das duas, usa-se o marcador embutido e o scan di-lo. |
| `DELONIX_ADVISORY_FEED` | `bins/delonix-runtime-bin/src/cmd/scan.rs:cmd_scan_update` | Origem para `delonix image scan --update`. | Um URL ou ficheiro (formato OSV ou nativo). | `--feed` ganha. Sem nenhum dos dois, o `--update` falha com um erro que nomeia ambos. |

## Afinação de recursos do Cloud Hypervisor e das VMs

Os tectos do libvirt abaixo são escritos no XML de domínio gerado
(`crates/adapters/delonix-vm/src/lib.rs:libvirt_domain_xml`); não mudam um domínio que já exista
até ele voltar a arrancar.

| Variável | Lida por | Finalidade | Valores / omissão | Notas |
|---|---|---|---|---|
| `DELONIX_HYPERVISOR_FW` | `crates/adapters/delonix-vm/src/lib.rs:default_ch_firmware` | Firmware que o Cloud Hypervisor arranca quando não é dado `--firmware`. | Um caminho de ficheiro, usado só se existir. Caso contrário, o primeiro que exista de `DEFAULT_CH_FIRMWARES` (o EDK2 `CLOUDHV.fd` antes do `hypervisor-fw`). | Ver o [Construir microVMs](microvm-setup.md) para saber porque é que o build do EDK2 vem primeiro. |
| `DELONIX_VM_RESERVE_MIB` | `crates/adapters/delonix-vm/src/lib.rs:vm_admission_check` | Memória mantida livre para o host ao admitir uma VM: uma VM é recusada se a sua memória mais esta reserva exceder o `MemAvailable`. | MiB; omissão `2048`; um valor que não se consegue interpretar recorre a 2048. | Baixá-la arrisca que o host mate processos por OOM. |
| `DELONIX_VM_MEM_HARD_LIMIT` | `crates/adapters/delonix-vm/src/lib.rs:mem_hard_limit_kib` | Se os domínios libvirt recebem um `<memtune><hard_limit>` sobre o processo QEMU inteiro. | `off` → sem limite rígido; qualquer outra coisa → ligado. | |
| `DELONIX_VM_MEM_OVERHEAD_PCT` | `crates/adapters/delonix-vm/src/lib.rs:mem_hard_limit_kib` | Margem acima da memória do convidado permitida pelo limite rígido. | Percentagem, intervalo aceite 5–200; omissão `25`; pelo menos 1 GiB de margem. Fora do intervalo → 25. | |
| `DELONIX_VM_CPU_QUOTA_CORES` | `crates/adapters/delonix-vm/src/lib.rs:cpu_quota_micros` | Tecto de CPU (`<cputune><quota>`) para um domínio libvirt, em cores. | Sem valor → vCPUs + 1 (o core extra é para as threads de emulador e de IO do QEMU). Um número positivo → esse número de cores. `off` → sem tecto. | |
| `DELONIX_VM_IO_MAX_BPS` | `crates/adapters/delonix-vm/src/lib.rs:vm_iotune_xml` | Tecto de débito do disco raiz (`<iotune><total_bytes_sec>`). | Bytes/s, inteiro positivo. Sem valor ou 0 → sem tecto (opt-in). | |
| `DELONIX_VM_IO_MAX_IOPS` | `crates/adapters/delonix-vm/src/lib.rs:vm_iotune_xml` | Tecto de IOPS do disco raiz (`<iotune><total_iops_sec>`). | Inteiro positivo. Sem valor ou 0 → sem tecto (opt-in). | |

### Orçamento de recursos dos containers

Estas afinam as omissões e o tecto agregado que o motor aplica aos containers
(`crates/adapters/delonix-linux/src/lib.rs`).

| Variável | Lida por | Finalidade | Valores / omissão | Notas |
|---|---|---|---|---|
| `DELONIX_RESERVE_PCT` | `crates/adapters/delonix-linux/src/lib.rs:host_reserve_pct` | Fracção do host (memória e CPU) que o slice de cgroup do motor pode usar no total. | Percentagem, intervalo aceite 10–95; omissão `85`. Fora do intervalo → 85. | É também a base das omissões por workload abaixo. |
| `DELONIX_DEFAULT_PCT` | `crates/adapters/delonix-linux/src/lib.rs:default_workload_pct` | Fracção do orçamento do motor que um workload pode tomar quando não declara limites. | Percentagem, intervalo aceite 1–100; omissão `25`. Fora do intervalo → 25. | A omissão de memória é de pelo menos 64 MiB; a omissão de CPU fica limitada entre 0,25 e 1,0 cores. |
| `DELONIX_SWAP_MAX` | `crates/adapters/delonix-linux/src/lib.rs:swap_max_value` | `memory.swap.max` do cgroup de um container. | Um valor de cgroup; omissão `0` (sem swap). `max` repõe o swap ilimitado. | O swap transforma um limite de memória num limite flexível. |
| `DELONIX_IO_MAX_BPS` | `crates/adapters/delonix-linux/src/lib.rs:host_io_max_bps` | Tecto agregado de leitura/escrita em disco (`io.max`) do slice do motor. | Bytes/s; omissão `500000000` (500 MB/s). `0` desliga-o. Um valor que não se consegue interpretar → omissão. | Um tecto de segurança contra um container saturar o disco, não QoS fino. Só se aplica onde o controlador `io` estiver disponível. |

## Providers

### Proxmox VE

Lidas uma vez no arranque da CLI por `bins/delonix-runtime-bin/src/cmd/vmbackends.rs:register_proxmox`.
Nada é contactado até um comando de VM seleccionar o backend `proxmox`. Valores em branco contam
como não definidos. Uma configuração errada imprime um aviso e deixa o backend por registar; não
impede os outros comandos. Contexto: `docs/adr/0008-proxmox-vm-backend.md`.

| Variável | Lida por | Finalidade | Valores / omissão | Notas |
|---|---|---|---|---|
| `DELONIX_PROXMOX_URL` | `cmd/vmbackends.rs:register_proxmox_with`; nomeada no erro de `crates/adapters/delonix-vm/src/lib.rs` (`KNOWN_UNREGISTERED`) | Endpoint da API do nó. Defini-la é o que activa o backend. | `https://<host>:8006`. | Exige `DELONIX_PROXMOX_NODE` e uma credencial. |
| `DELONIX_PROXMOX_NODE` | `cmd/vmbackends.rs:register_proxmox_with` | O nome do nó a usar (tal como o `GET /nodes` o reporta). | Por exemplo `pve`. Sem omissão: o backend nunca escolhe um nó por ti. | |
| `DELONIX_PROXMOX_SECRET` | `cmd/vmbackends.rs:proxmox_auth` | Nome de um `kind: Secret` que guarda a credencial. | Segredo com `tokenId`+`tokenSecret` (preferido) ou `username`+`password`. | Verificada **primeiro**. Preferível às variáveis abaixo, que acabam no histórico da shell e no `ps`. |
| `DELONIX_PROXMOX_TOKEN_ID` | `cmd/vmbackends.rs:proxmox_auth` | Id do token de API. | `user@realm!tokenname`. | Usada com `DELONIX_PROXMOX_TOKEN`; verificada depois do segredo. |
| `DELONIX_PROXMOX_TOKEN` | `cmd/vmbackends.rs:proxmox_auth` | Segredo do token de API. | | |
| `DELONIX_PROXMOX_TOKEN_FILE` | `cmd/vmbackends.rs:credential_value` | Caminho de um ficheiro com o segredo do token de API; preferível ao `DELONIX_PROXMOX_TOKEN`, que todo o processo filho herda. | Um caminho. | Recusado se alguém além do dono o puder ler (`chmod 600`). |
| `DELONIX_PROXMOX_USER` | `cmd/vmbackends.rs:proxmox_auth` | Conta para autenticação por password. | `root@pam`, … | Usada com `DELONIX_PROXMOX_PASSWORD`; verificada em último lugar. |
| `DELONIX_PROXMOX_PASSWORD` | `cmd/vmbackends.rs:proxmox_auth` | Password dessa conta. | | |
| `DELONIX_PROXMOX_INSECURE_TLS` | `cmd/vmbackends.rs:register_proxmox_with` | Salta a verificação do certificado TLS do nó. | `1`, `true` ou `yes` → salta; por omissão verifica. | **Outra máquina a responder em nome do nó recebe a credencial.** Só opt-in, nunca aplicada como recurso depois de um erro de TLS. |
| `DELONIX_PROXMOX_BRIDGE` | `cmd/vmbackends.rs:register_proxmox_with` | Bridge de omissão para as NICs das VMs neste nó. | Um nome de bridge; a omissão do backend é `vmbr0`. | Um `bridge:` por VM ganha. |
| `DELONIX_PROXMOX_VLAN` | `cmd/vmbackends.rs:parse_vlan` | Tag VLAN de omissão para as NICs das VMs neste nó. | 1–4094. Fora do intervalo é um **erro**, nunca descartado. | |

### TrueNAS

O provisionador TrueNAS (`kind: Volume` com `spec.provision.truenas`) obtém o seu alvo do manifesto
e de um `kind: Secret`, não de variáveis de ambiente. Os únicos nomes `DELONIX_TRUENAS_*` são
definições de teste — ver [Só para testes](#test-only).

## Observabilidade

| Variável | Lida por | Finalidade | Valores / omissão | Notas |
|---|---|---|---|---|
| `DELONIX_OTLP_ENDPOINT` | `crates/adapters/delonix-telemetry/src/telemetry.rs:build_otlp_layer` | Exporta spans de tracing por OTLP/HTTP (protobuf). | Um URL base como `http://localhost:4318`; `/v1/traces` é acrescentado se faltar. Sem valor ou em branco → sem exportador. | Uma falha a construir o exportador avisa e continua só com logs. O nome do serviço é `OTEL_SERVICE_NAME` ou o nome do executável. |
| `DELONIX_METRICS_ADDR` | `crates/interfaces/delonix-cri/src/lib.rs` (arranque do servidor CRI) | Activa um listener HTTP Prometheus `/metrics` no `delonix-cri`. | `host:port`, por exemplo `127.0.0.1:9100`. Sem valor → sem listener. | Um listener TCP: faz bind ao loopback, a menos que as métricas devam ser alcançáveis pela rede. |

## Definidas pelo próprio motor / internas

**Não as definas.** O motor escreve-as nos processos que arranca; defini-las à mão faz um comando
normal comportar-se como uma passagem interna.

| Variável | Lida por | Finalidade | Valores / omissão | Notas |
|---|---|---|---|---|
| `DELONIX_BIN` | `crates/contexts/delonix-node/src/dispatch.rs:cli_bin` (usada por `delonix-cri`, `delonix-mgmt`, `delonix-mcp`) | O executável `delonix` que um servidor volta a chamar para operações de ciclo de vida. | Um caminho. Sem valor: o `delonix` ao lado do executável do servidor, depois o `delonix` no `PATH`. | Definida por `delonix serve …` / `delonix mcp` (`cmd/serve.rs:exec_server`) com a CLI em execução. Defini-la à mão só é legítimo quando arrancas um binário de servidor directamente e queres que ele chame uma CLI específica. `scripts/cli-tree.sh` e `scripts/docs_cli_gate.py` também lêem uma variável com este nome para escolher o binário que inspeccionam (ver [Clonar, compilar e testar](build-and-test.md)). |
| `DELONIX_DISPATCH_VERSION` | `crates/contexts/delonix-node/src/dispatch.rs:check_version` (no `delonix-cri`, `delonix-mgmt`, `delonix-mcp`) | A versão que o servidor tem de ser; um servidor de outra release recusa-se a arrancar. | Definida por `cmd/serve.rs:exec_server` com a versão da CLI. | Um servidor arrancado directamente (por exemplo por uma unit do systemd) não tem expectativa nenhuma e não é verificado. |
| `DELONIX_REEXEC_ID` | `bins/delonix-runtime-bin/src/cmd/container.rs` (`cmd_run`, `reexec_env`) | Marca a segunda passagem de `container run/start --net <custom>` ou `--pod`, re-executada dentro dos namespaces do holder, e leva o id do container. | Definida por `cmd/container.rs:reexec_env`. | A sua presença salta verificações que a primeira passagem já fez (posse da porta). |
| `DELONIX_REEXEC_IP` | `bins/delonix-runtime-bin/src/cmd/container.rs` | O endereço SDN que a primeira passagem atribuiu, para a segunda passagem o registar. | Definida por `cmd/container.rs:reexec_env`. | |
| `DELONIX_PIN_SYNC` | `crates/adapters/delonix-sdn/src/pin_userns.rs` (`SYNC_ENV`) | Descritores de ficheiro dos pipes de handshake entre o chamador e o pin de rede enquanto os mapas do user namespace são escritos. | `<read-fd>,<write-fd>`. | Só definida quando o pin tem de criar namespaces novos; um pin que adopta nunca a recebe. |
| `DELONIX_DELEGATE_ATTEMPTED` | `bins/delonix-runtime-bin/src/cmd/kindmode.rs:reexec_under_delegated_scope` | Protege o único re-exec automático do `cluster create` sob um scope delegado, para que um host a quem ainda falte `cpu` mostre o erro real em vez de entrar em ciclo. | Definida a `1` no processo re-executado. | Para desligar o re-exec, usa `DELONIX_NO_AUTO_DELEGATE`. |
| `DELONIX_INTERNAL` | definida nos filhos por `crates/adapters/delonix-sdn/src/infra.rs` (`start_control` e os outros spawns do holder), `crates/adapters/delonix-vm/src/lib.rs:launch_vmm`, `crates/interfaces/delonix-cri/src/runtime_svc/lifecycle.rs`, `spdy.rs`, `streaming.rs` | Marca uma invocação máquina-a-máquina. | Definida a `1`. | **Nenhum leitor no código actual**: o comentário em `runtime_svc/lifecycle.rs:delonix` diz que ela "bypasses the grouped-commands barrier", mas já nada lê a variável. Continua a ser escrita, e aparece no environ dos processos de rede (ver os testes de `infra.rs:env_names_this_root`). |

## Em tempo de build

Valores de compilação: são fixados quando o binário é compilado e não se mudam definindo uma
variável quando o programa corre.

| Variável | Lida por | Finalidade | Valores / omissão | Notas |
|---|---|---|---|---|
| `DELONIX_GIT_HASH` | escrita por `bins/delonix-runtime-bin/build.rs`; lida com `env!` em `bins/delonix-runtime-bin/src/main.rs` | Hash curto do commit mostrado por `delonix --version`. | `git rev-parse --short=9 HEAD`, ou `unknown` sem git. | |
| `DELONIX_GIT_SINCE` | escrita por `bins/delonix-runtime-bin/build.rs`; lida em `src/main.rs` | Distância à tag mais recente, mostrada como `(+N commits since vX.Y.Z)`. | `<count>\|<tag>`, vazia numa tag ou sem git. | É assim que distingues duas builds com a mesma versão. |
| `DELONIX_BUILD_DATE` | escrita por `bins/delonix-runtime-bin/build.rs`; lida em `src/main.rs` e `src/cmd/man.rs` | Data de build no `--version` e nas man pages geradas. | `date -u +%Y-%m-%d`, ou `unknown`. | As man pages usam-na em vez do relógio para que duas corridas do gerador sobre a mesma build produzam a mesma saída. |
| `DELONIX_GIT_COMMIT` | lida com `option_env!` em `bins/delonix-runtime-bin/src/cmd/dockerapi.rs` (resposta de `/version`, `GitCommit`) | Commit reportado pela fatia da API Docker. | Tirada do ambiente de build se estiver definida; caso contrário `unknown`. | **Nada no repositório a define** (nem o `build.rs`, nem os workflows), por isso os binários publicados reportam `unknown`. |
| `DELONIX_BPF_OBJECT` | escrita por `crates/adapters/delonix-sdn/build.rs`; lida com `env!` em `crates/adapters/delonix-sdn/src/bpf.rs` | Caminho do objecto eBPF compilado de contabilização de fluxos, embutido no binário. | Definida só quando o `clang` e os headers da libbpf estão presentes em tempo de build (junto com `cfg(bpf_object)`). | Opcional: sem ela o runtime degrada para contadores do nftables. |
| `DELONIX_ASSET` | `scripts/install.sh` (secção do binário) | Variável de shell que guarda o nome do ficheiro do asset da release escolhido para este CPU (`delonix` ou a sua variante `-v3`). | Atribuída pelo script a partir do passo de download. | Não é lida do teu ambiente; defini-la antes de correres o instalador não tem efeito. |

## Só para testes

Estas são lidas apenas por testes. Sem elas, os testes ao vivo **saltam** e imprimem
`SKIP: … is not set` (um teste ao vivo que passasse em silêncio sem o seu alvo não provaria nada).

| Variável | Lida por | Finalidade | Valores / omissão | Notas |
|---|---|---|---|---|
| `DELONIX_PROXMOX_TEST_URL` | `crates/providers/delonix-proxmox/tests/live.rs:target` | Nó Proxmox contra o qual correr os testes ao vivo do backend. | `https://<host>:8006`. | Activa os testes. Criam e destroem uma VM; corre-os só contra um nó que seja teu. |
| `DELONIX_PROXMOX_TEST_NODE` | `crates/providers/delonix-proxmox/tests/live.rs:target` | Nome do nó. | Omissão `pve`. | |
| `DELONIX_PROXMOX_TEST_USER` | `crates/providers/delonix-proxmox/tests/live.rs:target` | Conta para autenticação por password. | Por exemplo `root@pam`. Obrigatória. | A verificação TLS está desligada nestes testes. |
| `DELONIX_PROXMOX_TEST_PASS` | `crates/providers/delonix-proxmox/tests/live.rs:target` | A sua password. | Obrigatória. | |
| `DELONIX_PROXMOX_TEST_STORAGE` | `crates/providers/delonix-proxmox/tests/live.rs` | Storage para o disco da VM de teste. | Omissão `local-lvm`. | |
| `DELONIX_PROXMOX_TEST_AGENT_VMID` | `crates/providers/delonix-proxmox/tests/live.rs:o_ip_vem_do_agente_de_um_convidado_a_serio` | Uma VM existente, com o QEMU guest agent a correr, cujo IP o teste lê. | Um id de VM. | Saltado quando não definida, mesmo com o URL definido. |
| `DELONIX_TRUENAS_TEST_URL` | `crates/providers/delonix-truenas/tests/live.rs:target` | Appliance TrueNAS contra o qual correr os testes ao vivo do provisionador. | `https://<host>`. | Activa os testes. Criam e destroem `<pool>/dlxlive-<pid>`. |
| `DELONIX_TRUENAS_TEST_POOL` | `crates/providers/delonix-truenas/tests/live.rs:target` | Pool para o dataset de teste. | Omissão `tank`. | |
| `DELONIX_TRUENAS_TEST_KEY` | `crates/providers/delonix-truenas/tests/live.rs:target` | Chave de API. | | Usada em vez de utilizador e password quando definida. |
| `DELONIX_TRUENAS_TEST_USER` | `crates/providers/delonix-truenas/tests/live.rs:target` | Conta para autenticação por password. | Obrigatória sem chave. | A verificação TLS está desligada nestes testes. |
| `DELONIX_TRUENAS_TEST_PASS` | `crates/providers/delonix-truenas/tests/live.rs:target` | A sua password. | Obrigatória sem chave. | |
| `DELONIX_UPDATE_FIXTURES` | `crates/adapters/delonix-linux/tests/advisor_fixtures.rs:goldens_match_the_rules_as_they_are_today` | Reescreve as fixtures golden do advisor em `crates/adapters/delonix-linux/tests/fixtures/advisor/` em vez de comparar com elas. | Definida (qualquer valor) → reescreve. | Regenera-as no **mesmo commit** que a mudança de regra. |

Corre os testes ao vivo (substitui os marcadores; nunca faças commit de credenciais reais):

```bash
DELONIX_PROXMOX_TEST_URL=https://<node>:8006 \
DELONIX_PROXMOX_TEST_NODE=pve \
DELONIX_PROXMOX_TEST_USER=root@pam \
DELONIX_PROXMOX_TEST_PASS='<password>' \
  cargo test -p delonix-proxmox --test live -- --nocapture

DELONIX_TRUENAS_TEST_URL=https://<appliance> \
DELONIX_TRUENAS_TEST_USER=<user> \
DELONIX_TRUENAS_TEST_PASS='<password>' \
DELONIX_TRUENAS_TEST_POOL=tank \
  cargo test -p delonix-truenas --test live -- --nocapture

DELONIX_UPDATE_FIXTURES=1 cargo test -p delonix-linux --test advisor_fixtures
```

## Nomes que parecem variáveis mas não são

Alguns nomes `DELONIX_*` aparecem no código sem serem lidos do ambiente de nenhum processo. Estão
excluídos da verificação acima por `NOT_ENV` em `scripts/dev_docs.py`, cada um com a sua razão, e
nunca precisas de os definir:

- `DELONIX_CRI_SOCKET` — uma constante Rust em `bins/delonix-runtime-bin/src/cmd/cluster.rs`,
  passada a `kubeadm … --cri-socket=`. O socket onde o servidor CRI escuta é `DELONIX_CRI_ADDR`.
- `DELONIX_ROOTX`, `DELONIX_ROOT_BACKUP` — fixtures de teste em `crates/adapters/delonix-sdn/src/infra.rs`,
  que provam que a leitura do environ de um processo compara nomes inteiros, não prefixos.
- `DELONIX_IMAGE`, `DELONIX_DISTRO`, `DELONIX_RELEASE`, `DELONIX_BUILT_BY`, `DELONIX_BASE_IMAGE`,
  `DELONIX_BASE_SHA256`, `DELONIX_K8S_VERSION`, `DELONIX_OFFLINE`, `DELONIX_NODE_EXPORTER`,
  `DELONIX_EXTRA_PACKAGES` — chaves do ficheiro de proveniência `/etc/delonix-image-release` que o
  `image vm build` escreve **dentro** de uma imagem de VM construída
  (`bins/delonix-runtime-bin/src/cmd/vmimage.rs`). Lê-as no convidado com
  `cat /etc/delonix-image-release`; nenhum processo as lê.

---

**Seguinte:** [Glossário](glossary.md) — os termos que encontras neste repositório, com o seu significado no Delonix e onde cada um é explicado.
