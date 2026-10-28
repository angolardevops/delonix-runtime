# Changelog

Todas as mudanças notáveis do **Delonix** (antes `delonix-runtime`). O formato
segue [Keep a Changelog](https://keepachangelog.com/pt-BR/1.1.0/) e o projecto a
política de versões descrita em [VERSIONING.md](VERSIONING.md).

As notas em prosa de cada release histórica vivem em `docs/releases/v<tag>.md`
(consolidadas em `docs/RELEASES.md`); este ficheiro é o resumo categorizado.

> **Linhas de versão.** O produto renomeou-se de `delonix-runtime` para
> **Delonix** e **re-baseou o número de versão de propósito**: a linha
> `delonix-runtime` (v1.0.0 … v5.1.1) está congelada e passa a **histórico**; a
> linha **Delonix** abre em `0.1.0`. `0.1.0` ser inferior a `5.1.1` não é uma
> regressão — é uma linha nova. As capacidades herdadas da linha anterior **não**
> são trabalho novo da `0.1.0`; ver a secção histórica abaixo.

---

## [Não lançado] — linha Delonix 0.x (transição de identidade)

Preparação da primeira release oficial Delonix `0.1.0`. Nada aqui foi ainda
etiquetado nem publicado.

### Changed
- Identidade do produto passa a **Delonix**; o repositório migra para
  `github.com/ngolacloud/delonix`. O binário da CLI já se chamava `delonix`.
- `scripts/version_gate.py` passa a ser **consciente de linha de versão**: a
  linha legada `delonix-runtime` (majors 1..5) e a linha Delonix (0.x) são
  comparadas em separado, com guarda de que nenhum commit perde o histórico
  enviado (tem de conter a tag `v5.*` mais recente). Coberto por
  `scripts/test_version_gate.py` (novo).

### Added
- `CHANGELOG.md` e [VERSIONING.md](VERSIONING.md) (política de compatibilidade
  pré-1.0 e mecânica da transição de linha).
- Adendo ao ADR-0061 a trazer o template `rust` (axum/tokio) para o contrato do
  `delonix init`, com verificação medida (gerar → compilar → correr → health 200).

### Preservado (NÃO renomear na transição)
- Directórios de estado (`/var/lib/delonix`, `/etc/delonix`,
  `~/.local/share/delonix`), sockets (`/run/delonix-*.sock`), serviços systemd,
  variáveis `DELONIX_*` e grupos de schema `*.delonix.io` — contratos
  operacionais inalterados entre a linha 5.x e a 0.x.

---

# Histórico — linha `delonix-runtime` (v1.0.0 – v5.1.1)

Capacidades herdadas pela linha Delonix. Datas = data real da tag.

## [5.1.1] - 2026-10-15
### Changed
- A flag `--l18n` perde `global`, deixando de ser aceite depois de um subcomando (passa a `rc=2` no fim de um comando normal); funciona antes do subcomando ou via `DELONIX_L18N`.
- O `install.sh` imprime no fim uma linha `downloads and documentation:` com o endereço da release.
- `Cargo.toml` sobe para `5.1.1` e as páginas de `docs/comandos/` são regeneradas.
### Added
- `build.rs` lê a chave `tls=` do `template.meta`; `scaffold.rs` ganha `template_supports_tls` e `template_has_version` (ainda sem chamador na tag).

## [5.1.0] - 2026-10-15
### Changed
- O CRI passa a ter tecto de capabilities por omissão (`KEPT_CAPS` em modo `reject`), recusando `privileged: true` ou `capabilities.add` acima dele no `CreateContainer`.
- `UpdateContainerResources` implementado e sensível ao lugar (scope do kubelet vs leaf), com fusão campo a campo.
- Hotplug a quente de CPU e memória para Cloud Hypervisor, libvirt e Proxmox, reiniciando em vez de substituir a VM.
- Os onze templates do `delonix init` nascem com testes, dois CIs, Conventional Commits, SonarQube e secção de arquitectura, com portão a exigi-lo.
- A cobertura de folhas da CLI executadas e julgadas pela bateria sobe de 126/272 para 258/270 (95,6%).
### Added
- Pools de armazenamento (ADR-0067) com helper root por socket e drivers `btrfs`, `zfs` e `lvm-thin`.
- `VirtualMachineService` servido na API de nó (ADR-0040 P5), com activação por socket e saída por inactividade.
### Fixed
- `secret rotate-key` já não faz a versão recuar para 1 (ACH-034).
- Uma rota de rede passa a ser utilizável a partir de uma rede adicional (#733); um arranque falhado já não desfaz a rede de um container a correr.
### Security
- O portão de admissão falha fechado perante uma falha de scan, não só perante achados (#778).
- Guarda opcional `denyPrivateEgress` mantém cargas fora de espaço privado e CGNAT; extracção de layer ganha tecto de entradas e bytes contra bombas de descompressão.
### Removed
- `image vm …`, `image --vm …` e as formas planas `vm build|convert|pull|push|ls-remote` saem sem alias; `delonix vm image <verbo>` é o único caminho (#772).

## [5.0.2] - 2026-10-11
### Fixed
- Uma `NetworkRoute` aberta passa a servir containers ligados a mais de uma rede via `network connect`, instalando rota e rota espelhada por protocolo próprio (#733).

## [5.0.1] - 2026-10-10
### Security
- `--secret-files` aborta o init do container (exit 126) quando o tmpfs não monta, em vez de escrever o segredo na camada persistente (`3626ae70`).
- A admissão por scan (`DELONIX_SCAN_ON_PULL`) recusa o pull e remove a imagem em qualquer falha do scan; só `DX-1401` (imagem sem gestor de pacotes) deixa passar (#778).

## [5.0.0] - 2026-10-08
### Changed
- BREAKING: um container corre como o `USER` da imagem em vez de uid 0 (ADR-0062); sem `--user` o processo deixa de ser root, recuperável com `--user 0`.
- BREAKING: `--user` deixa de reapropriar o filesystem inteiro, aplicando apenas o índice de donos da imagem (odoo passou de 16 min para 1,7 s).
- BREAKING: as assinaturas `can_bind_host_port` e `host_port_busy` do `delonix-sdn` passam a receber um `Proto` (#718).
- O endereço de bind de uma porta publicada passa a ser registado em `addr:hostPort:contPort/proto` e mostrado no `container ls` (#718).
- O bloco `provider` passa a `type` + `spec` tipado inline (ADR-0070/0071); `kind: NetworkZone` passa a `kind: Network` com `spec.provider.proxmox`; grafias antigas carregam como superadas.
### Added
- `kind: SystemContainer` para LXC do Proxmox (ADR-0058) com dia 2 completo; `kind: StoragePool` com driver `dir` (ADR-0067 P0).
- `delonix serve node-api` serve o contrato de nó em gRPC e HTTP/JSON num socket unix, com erros RFC 9457 e OpenAPI gerado (ADR-0042).
- Segredos versionados com `secret apply` a reportar `created`/`unchanged`/`rotated to version N`; providers de rede por papel (`GatewayProvider`, `SegmentProvider`).
- `hostPort` publicado em modo root/CNI para tcp, udp e sctp (ADR-0074); appliance FreePBX 17 + Asterisk 22.
### Security
- Uma namespace nomeada num host sem `br_netfilter` é recusada (`DX-6305`, exit 69), com válvula `DELONIX_ALLOW_UNENFORCED_ISOLATION=1`.
- O anti-spoofing muda para tabela `bridge` onde realmente corta (MAC, origem IPv4 e emissor ARP por porta); um workload governado por política nasce fechado.
- Credenciais de laboratório (`opnsense-lab.env`) passam a estar ignoradas pelo git (#716).
### Fixed
- Um campo desconhecido recusa o manifesto inteiro antes de qualquer efeito (ADR-0069), incluindo dentro de `spec.containers[]`; um store ilegível é erro e não máquina vazia.
- Um `rm -f` que desistiu não é ressuscitado pelo supervisor; o `slirp4netns` de um container termina com ele (#646, #648, #651).

## [4.5.0] - 2026-09-28
### Changed
- A firewall por container falha fechada: uma spec com regra, `dir`, `action` ou política inválida é recusada inteira em vez de aplicada parcialmente.
- Uma falha de isolamento de namespace passa a ser erro (não aviso) em `container run`, `start` e `pod create`, desfazendo o attach.
- `--cpuset`, `--io-weight` e `--device-read/write-bps/iops` são recusados (exit 69) quando o cgroup não tem o controlador, com válvula `DELONIX_ALLOW_UNENFORCED_LIMITS`.
- `container rm -f` só volta quando o processo morreu; passado 30 s mantém o registo e devolve `DX-8101` (exit 124).
- Proxmox: cobertura da API medida sobe de 16 (2,4%) para 161 rotas (23,9%); erros tipados por status HTTP e livro de tarefas por VM.
- Pull e push de imagens em streaming sem a imagem inteira em memória (pico de RSS do pull de `node:22`: 214 MB → 27 MB).
### Added
- Verbos `vm resize`, `vm cloud-init`, `vm move`, `provider ls|describe|matrix|config`, `net netns gc`, `serve node-api`; Kinds `NetworkGateway` e `NetworkZone`.
- Ficheiro `providers.yaml` por nó (ADR-0054); catálogo de capacidades versionado (ADR-0050); ferramenta MCP `workload.usage`.
- `vm create --require <capacidade>`, `--allow-mac-spoofing` e arranque de imagem do store do motor no Proxmox (ADR-0057).
### Fixed
- `network rm` recusa uma rede com algo ligado (`DX-5307`, exit 5) e nomeia os consumidores; IPAM transaccional recusa reserva de IP de outro dono (`DX-5308`).
- O pool de DHCP das VMs passa a viver dentro da rede, servindo o CIDR correcto; o binário já não morre por SIGPIPE numa escrita de socket de rede (#573).
- `system df` conta o store inteiro (190 GiB antes reportavam 105); os testes deixam de sujar o `/tmp`.

## [4.4.0] - 2026-09-23
### Added
- `delonix drift` compara o carimbo do último apply contra o observado, com manifesto opcional (M05).
- `compatibility compose` (28 servidas, 12 recusadas, 49 em falta de 89 chaves) e `migrate assess -f docker-compose.yml` (M02).
- `kind: RuntimePolicy`, o tecto de admissão em forma declarativa que `stack apply` só sobe ou aperta (M04); Kinds no manifesto 19 → 20.
- `RUN --mount=type=cache` e SBOM SPDX 2.3 automático por imagem construída (M03); `POST /images/create` na Docker API.
- `scripts/bench_gate.py` com três saídas, incluindo `3` quando se recusa a julgar por ruído da bancada.
### Changed
- BREAKING para quem publica: empurrar uma tag já não publica; a tag e a release passam a ser duas decisões via `gh workflow run` (#472).
### Security
- Job de fuzz em CI encontrou e corrigiu um OOM por bomba de expansão em `parse_dockerfile`, com tecto de 1 MiB em `substitute_vars` (#461, #463).

## [4.3.0] - 2026-09-21
### Added
- Grupo `delonix hosts` (não estável) com `hosts sync` para publicar nomes de serviço no `/etc/hosts` (ADR-0048 fase 2).
### Changed
- O `install.sh` passa a aceitar `aarch64`/`arm64`, compondo o nome do asset de `uname -m`; é o primeiro release cujo instalador o aceita.
- Fora de x86_64 não se descarregam Cloud Hypervisor, EDK2 nem `hypervisor-fw`, e o backend de VM é o libvirt.
### Fixed
- Notas da v4.2.0 corrigidas: a VM libvirt e `kind: IPPool` já estavam construídos (fases 2 e 3 do ADR-0046).

## [4.2.0] - 2026-09-21
### Added
- `delonix vm build` e o ficheiro `vm.yaml` (o "compose" do `VMfile`), com rotas por distro e por `appliance:` (#451).
- `VirtualMachine.spec.expose` publica serviço HTTP/S de VM por nome; `kind: IPPool` para reservas de endereços do host (ADR-0046).
- Nome padrão de serviço `<nome>.<ns>.svc.delonix.internal` e coluna `SVC` em `container/vm/stack ls` (ADR-0048 fase 1).
- Appliances de monitorização, Carbonio CE, GLPI 11 e Wazuh; `install.sh --performance`.
- Binários aarch64 (`delonix`, `-cri`, `-mcp`, `-mgmt`) publicados pela primeira vez.
### Changed
- `delonix-cri`, `delonix-mcp` e `delonix-mgmt` respondem a `--version`/`-V` e saem em vez de arrancar o servidor.

## [4.1.1] - 2026-09-20
### Security
- `vm migrate`: injecção de shell e colisão de isolamento por namespace corrigidas (CRÍTICOS); `kubeadm join` valida o output remoto antes de o passar a um shell.
- `metadata.name` de cluster validado antes de compor caminhos (um nome com `..` escrevia a CA do etcd fora da raiz); `ExecSync` do CRI ganha tecto de tamanho.
### Added
- `vm destroy` (teardown entre providers); `vm create --rm` / `--rm-after` para VMs descartáveis; `--cri-bin` em `cluster apply` e `cluster kubeadm`.
### Fixed
- `prepare_host` compara o `sha256` do CRI em vez de só "o serviço está vivo", garantindo que uma correcção chega a um nó existente (#242).
- `resolve_cri_bin` deixa de compilar o código-fonte perto do cwd; só com `DELONIX_CRI_FROM_SOURCE=1` (#243).

## [4.1.0] - 2026-09-19
### Fixed
- Um nó root fresco volta a aceitar `-m`/`--cpus`/`--cpu-weight`: `cgroup_limits_apply()` confirma que os controladores estão em `cgroup.subtree_control` e nomeia o controlador em falta em vez de reportar "Permission denied" (#415).
### Changed
- ADR-0043 fecha nos últimos cinco crates (`delonix-vm`, `-truenas`, `-proxmox`, `-linux`, `-sdn`); o dicionário `DX-CDNN` passa de 61 para 198 códigos e o ADR passa a Accepted.

## [4.0.0] - 2026-09-17
### Changed
- BREAKING: `delonix-scan`→`delonix-scanner`, `delonix-image`→`delonix-oci`, `delonix-net`→`delonix-sdn`, `delonix-runtime`→`delonix-linux` (ADR-0040 fase P3).
- Reestruturação do motor em camadas (ADR-0040): contextos, ports e adaptadores; `serve cri`/`serve api`/`mcp` passam a executar binários próprios em vez de os embutir.
### Removed
- `delonix-runtime-core` eliminado; tipos partilhados foram para `delonix-model` e o estado persistido para o novo `delonix-state`.
### Added
- Dicionário de códigos `DX-CDNN` (ADR-0043) com `delonix explain <código>`; `delonix compatibility docker`.
- `delonix init` adopta um directório existente em vez de despejar exemplos; `network ipam ls`/`prune`.
### Fixed
- Um nó root novo voltou a arrancar pods (#310); o control-plane de nó único deixou de entrar em crash-loop ao declarar `cgroupDriver: cgroupfs` (#315).
- O `boot` do cloud-hypervisor só devolve quando o VMM confirma a VM a correr (#350); `POST /wait` devolve o código de saída real.

## [3.1.0] - 2026-09-11
### Fixed
- A sonda de `--health-cmd` deixava dois zombies por execução, esgotando o `pids.max`; o watchdog passa a reapar o próprio filho.
- Um `stop` pedido podia ficar `Crashed`; `start`/`restart` perdiam o supervisor e o exit code; `container healthcheck` ignorava a sonda do próprio container.
- `image load` apagava os outros nomes da mesma imagem; `secret create` sobre um segredo existente apagava chaves não repetidas.
- `stack apply` adoptava um recurso alheio dizendo "nothing to do"; o tecto de recursos cobria só o PID 1.
### Changed
- README e `delonix --help` alinhados com a reestruturação da CLI (33 grupos, COMMAND MAP categorizado), com dois gates novos (ACH-032, ACH-033).

## [3.0.0] - 2026-09-09
### Removed
- `image build|convert|import|init|ls-remote` (formas `--vm`), `sharevolume apply|describe|ls|rm|migrate`, `storage create|ls`, `pod ls` e `vm status` saem sem alias (249 → 244 folhas).
### Added
- `kind: Service` (ADR-0032) e `kind: App` com Cloud Native Buildpacks (ADR-0035); `image sign` (ECDSA-P256 cosign), `cluster drain`/`uncordon`/`upgrade`, `vm migrate`, `vm pause`/`unpause`, `net capture`, `pod port-forward`.
- `docker compose` completo: `profiles:`, `extends:`, `build.target`, `deploy.replicas`, `ipv4_address`, `configs:`/`secrets:`, volumes anónimos e multi-ficheiro.
### Fixed
- `PodSandboxStatus` devolvia `linux: null` e nenhum `kubeadm` subia (#233); um container privilegiado levava os `masked`/`readonly paths` e derrubava o `kube-proxy` (#239).
- O `mount(2)` truncava o `lowerdir=` acima de ~4 KB; o overlay passa para a API `fsopen`/`fsconfig`/`fsmount` (ADR-0037).

## [2.0.0] - 2026-09-02
### Changed
- `image list`/`backup list` revertem para `image ls`/`backup ls` sem alias (a grafia `list` falha com `unrecognized subcommand`, rc=2); `image remove` fica.
### Added
- `delonix system resources` reporta os controladores de cgroup realmente aplicáveis, a pressão PSI e achados `DLX-RES-nnn`; `system regulate` corta e repõe `cpu.weight` e `memory.high` por causa.
- `delonix diff <kind> <name>`, `cluster health`, `cluster describe`, `system metrics`, `config get|set|unset`, `secret rotate`; `get`/`describe`/`delete` alcançam `NetworkRoute` e `NetworkPolicy`.
- Novas colunas em `volume`/`network`/`pod`/`secret`/`image`/`container`/`cluster ls` (SIZE, USED BY, AGE, RESTARTS, ORPHAN, K8S VERSION).
### Fixed
- O tecto agregado passa a existir em rootless no `dlx-containers` delegado; `image remove alpine:latest` deixa de desmarcar uma imagem de que um container vivo depende.

## [1.1.0] - 2026-08-30
### Added
- `delonix-security-runtime`: o caminho de VM passa pela admissão antes de resolver/descarregar, com guardas `denyDevicePassthrough`, `denyLatestVmImage` e `allowedImageUrlHosts` (ADR-0026).
- `delonix mcp serve|doctor|capabilities`: servidor MCP local stdio para agentes, com `confirm` explícito acima de `SAFE_WRITE`.
### Security
- As guardas de VM nascem desligadas para não partir frotas; o nó aponta a metade aberta com um lint silenciável por `DELONIX_POLICY_LINT=0`.
### Fixed
- Um `exec` logo após `run -d` podia correr no filesystem do host; corrigido esperando por `setup_rootfs`.
- O `exec` do CRI matava por número de pid (podia ceifar outro processo); passou a `pidfd`.
- Nove erros diziam "não existe" a coisas que existem (padrão `ENOENT` enganador); `kind: Ingress` deixa de descartar `tls[].hosts` e outros campos em silêncio.
### Removed
- `storage dash|inspect|rm` colapsam em `volume` (grupo não estável); `storage create|ls` ficam.

## [1.0.0] - 2026-08-29
### Added
- Primeira release estável: `docs/cli-stability.md` passa a ser contrato de semver.
- Motor de containers e microVMs rootless-first e daemonless em Rust: ciclo de vida completo, pods multi-container, GPU por CDI, build com Dockerfile/Delonixfile, SDN rootless, VMs (Cloud Hypervisor/libvirt/Proxmox), `delonix-cri`, `cluster kubeadm` e infraestrutura declarativa por Kinds.
### Changed
- As superfícies públicas passam a dizer "Delonix Engine"; o nome do repositório e os doc-comments internos mantêm "Delonix Runtime".
### Removed
- Os seis atalhos de topo `ps`, `run`, `exec`, `logs`, `rm` e `images` saem sem alias; usar `container <verbo>` e `image list`.
### Fixed
- `image rm`, prometido estável mas morto desde o B2 (passou a `image remove` sem alias), corrigido na mesma janela.

---

Versões anteriores à `v1.0.0` (linha `v0.x` de desenvolvimento) em
`docs/releases/archive/`.
