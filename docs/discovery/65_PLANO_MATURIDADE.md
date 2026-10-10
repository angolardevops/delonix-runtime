# Plano de maturidade do delonix-runtime — todos os domínios que o motor promete

> **O que este documento é.** O plano para levar cada domínio que o motor
> declara oferecer a um nível em que se pode pôr um nó crítico em cima dele.
> Cada item tem um **critério de saída medido**. Um item sem medição não fecha.
>
> **O que não é.** Uma lista de desejos nem uma estimativa de calendário. Os
> tamanhos (P/M/G) ordenam o trabalho; não prometem datas.
>
> **Relação com o que já existe.** Não substitui o programa das 13 melhorias
> (`docs/roadmap/13-improvements-traceability.md`, M01–M13). Esse programa
> organiza o trabalho por *eixo de produto*. Este organiza-o por *domínio
> funcional*, com o catálogo de capacidades do ADR-0050 como denominador. Cada
> item abaixo diz a que M pertence. O estado continua a ser registado na matriz
> de rastreabilidade, no mesmo commit que o muda.

| Campo | Valor |
|---|---|
| Baseline medida em | **2026-10-02** |
| Contra | `origin/main` `a0d683c6`, motor **4.5.0**, catálogo de capacidades **1.3.0** |
| Fonte da matriz | `docs/providers/capability-matrix.md` (gerado por `delonix provider matrix`) |
| Compatibilidade | `delonix compatibility docker` e `delonix compatibility compose` (binário 4.5.0) |

---

## 1. O que «maduro» quer dizer aqui

«Maduro» não é uma impressão. É uma propriedade verificável de cada **célula**
(capacidade × provider que a declara) do catálogo:

| Nível | Nome | Exige |
|---|---|---|
| **N0** | Em falta | `not-implemented` |
| **N1** | Implementado | código e teste unitário; na matriz `partial` |
| **N2** | Provado | `supported`, com evidência nomeada (`check:`, `e2e:`, `chaos:`, `live:`) que o gate de evidência confirma existir |
| **N3** | Produção | N2 **e** o caminho de FALHA também provado (cenário de caos ou check de recusa com classe de exit code), **e** observável (evento ou métrica que diga que falhou), **e** documentado para o utilizador |
| **N4** | Garantido | N3 **e** corrido de forma recorrente num runner que o executa de facto (não `skipped`), com regressão a ficar vermelha |

**Um domínio é maduro quando todas as suas células aplicáveis estão em N3**, e o
domínio inteiro corre num job recorrente (N4) com o resultado publicado.

Duas regras que este repositório já pagou caro, e que valem para todo o plano:

1. **Verde por ausência de execução não conta.** O job de caos dos runners
   alojados do GitHub fica verde a saltar tudo, porque bloqueiam user namespaces
   não privilegiados. Uma célula só é N4 se o log mostrar que o caso correu.
2. **«Feito» = fundido + gate + prova real.** Uma lista de «não validado» não
   fecha um item. Ou se mede, ou o item fica aberto com a razão.

---

## 2. Baseline medida

### 2.1 Os dois números, e porque são diferentes

| Métrica | Valor | O que mede |
|---|---|---|
| Capacidades com prova no **melhor** provider | **73 de 141 (52 %)** | «o motor sabe fazer isto em algum lado» |
| Capacidades com prova ou parciais, melhor provider | **115 de 141 (82 %)** | «está escrito, com ou sem prova» |
| **Células** com prova (capacidade × provider aplicável) | **100 de 252 (39,7 %)** | «funciona provadamente onde promete» — **é a métrica deste plano**, e desde o F0.1 é CALCULADA pelo `delonix provider matrix` a partir do catálogo em código, não contada à mão sobre o markdown (o «99 de 265» de 2026-10-02 e o «111 de 281» do plano 66 eram as duas contagens manuais anteriores; a segunda vinha de um `grep -c` que contava os sumários do próprio ficheiro) |

A métrica de célula é a honesta: uma capacidade provada no Proxmox e só
`partial` no libvirt não está madura para quem corre libvirt.

### 2.2 Por domínio (melhor provider)

| Domínio | Capac. | N2+ | N1 | N0 / externo |
|---|---|---|---|---|
| Protecção | 9 | 9 (100 %) | 0 | 0 |
| Containers | 12 | 8 (67 %) | 3 | 1 |
| Computação de VMs | 25 | 15 (60 %) | 9 | 1 |
| System containers | 14 | 7 (50 %) | 4 | 3 sem API no provider |
| Mobilidade | 4 | 2 (50 %) | 0 | 2 externos |
| Consola | 4 | 2 (50 %) | 2 | 0 |
| Rede | 43 | 21 (49 %) | 9 | 13 |
| Firewall | 8 | 3 (38 %) | 4 | 1 |
| Armazenamento | 12 | 4 (33 %) | 4 | 4 |
| Inventário | 4 | 1 (25 %) | 3 | 0 |
| Métricas | 4 | 1 (25 %) | 2 | 1 |
| Acesso | 2 | 0 (0 %) | 2 | 0 |

### 2.3 Por provider (células)

| Provider | N2 | N1 | N0 | externo |
|---|---|---|---|---|
| proxmox (compute) | 32 | 13 | 9 | 3 |
| libvirt | 16 | 24 | 10 | 2 |
| cloud-hypervisor | 12 | 18 | 9 | 2 |
| linux (containers) | 11 | 9 | 3 | 0 |
| linux (rede) | 8 | 14 | 6 | 1 |
| proxmox (rede) | 6 | 7 | 13 | 2 |
| opnsense (gateway) | 10 | 0 | 12 | 2 |
| linux (armazenamento) | 4 | 3 | 2 | 2 |

### 2.4 Superfícies fora da matriz

| Superfície | Medida | Data |
|---|---|---|
| CRI (`critest` v1.36.0) | 79 de 103 | 2026-08-25, motor 0.63.1 — **19 versões atrás, por remedir** |
| Docker Engine API | 15 rotas servidas, 11 recusadas com razão | 2026-10-02 |
| Compose, por serviço | 28 servidas, 12 recusadas, **49 em falta**, de 89 | 2026-10-02 |
| Compose, topo | 7 servidas, 1 recusada, de 8 | 2026-10-02 |
| Contrato de nó | 1 RPC servido (`ListProviders`) → **2026-10-10**: Node, Network, leituras de Volume, VirtualMachine (menos `Console`) e Operations servidos | 2026-10-02; remedido 2026-10-10 |
| CLI executada pela bateria | **37 %** (91 de 244 folhas; medido no cabeçalho do `scripts/e2e.sh`); 63 % no ramo `auditoria-cobertura`, por integrar → **2026-10-10: 65,8 %** (181 de 275, `scripts/cli_exec_trace.tsv`; o ramo perdeu-se e a cobertura foi refeita) | 2026-09-09, v3.0.0; remedido 2026-10-10 |
| Checks na bateria E2E | 735 linhas `check` | 2026-10-02 |
| Cenários de caos | 25 | 2026-10-02 |
| Testes no workspace | 2658 | 2026-10-02 |
| ADRs | 66 | 2026-10-02 |

---

## 3. A causa comum: porque é que 63 % das células não estão provadas

Medido: há **88 células `partial`**, **64 `not-implemented`** e **14
`requires-external-component`**. Lida a matriz linha a linha, as células abaixo
de N2 caem em **quatro causas**. A divisão das 88 parciais por C1–C3 é uma
leitura da razão escrita em cada célula, por isso é aproximada; o total não é.
O plano ataca as causas, não as células uma a uma:

| Causa | Células afectadas (aprox.) | Exemplo | Remédio |
|---|---|---|---|
| **C1 — Implementado, sem check que o nomeie** | ~45 das 88 parciais | `vm.destroy`/`vm.restart` no CH e libvirt; `vm.pause` no CH; `pod.shared-ipc-uts` | Escrever o check. Barato e de alto retorno (Onda 1) |
| **C2 — A bateria nunca entra num convidado** | ~15 das 88 parciais | `vm.cloud-init`, `vm.ip.observed`, `vm.console.serial`, `vm.guest-agent` em libvirt/CH | Imagem de teste mínima que arranca e responde, e um runner com KVM (F0.3) |
| **C3 — Precisa de hardware ou infra que o laboratório não tem** | ~28 das 88 parciais | PCI passthrough (IOMMU), GPU/CDI, NFS com CAP_SYS_ADMIN, fabric multi-nó, overlay entre dois nós | Laboratório dedicado (F0.3), ou declarar fora de âmbito |
| **C4 — Não implementado** | 64 (medido) | `vm.hotplug`, `pod.shared-pid`, `net.lb.l4`, `storage.lvm-thin`, `firewall.icmp-type`, `host.capacity` | Implementar **só** o que passar pela decisão de âmbito (§5) |

**Consequência para a ordem do plano**: C1 e C2 levam o motor de 37 % para
perto de 60 % de células provadas sem uma linha de funcionalidade nova. Fazer
funcionalidade nova antes disso aumenta o denominador e baixa a percentagem.

---

## 4. Fases

### F0 — Fundação de medição (antes de tudo o resto)

Sem isto, nenhuma percentagem posterior é confiável.

| # | Item | Tamanho | Critério de saída | M |
|---|---|---|---|---|
| F0.1 | `delonix provider matrix` passa a imprimir a **métrica de célula** (N2 / aplicáveis, por domínio e por provider) e o gate falha se ela descer sem a linha de base baixar no mesmo commit (ratchet nos dois sentidos, como o `lang_ratchet`) | P | número no topo do `capability-matrix.md`; teste que chumba com uma célula `supported` rebaixada | M13 |
| F0.2 | **Ratchet de execução da CLI** — **o instrumento FEITO a 2026-10-07** (`scripts/cli_exec_ratchet.py`, job `cli execution ratchet`): conta as folhas que um `check`/`xfail` de uma corrida REAL invocou **e julgou**, derivadas do `results.jsonl`, contra o inventário do `cli-tree.sh`; ratchet nos três sentidos (desce, sobe sem a base subir, ou entra folha nova sem decisão). Numerador de um trace commitado com proveniência, porque o runner não corre a bateria. **Medido: 125 de 272 — 46,0 %** (4.5.0+185, PASS=1104 FAIL=0 SKIP=12); o «37 %» anterior era contagem à mão sobre outra árvore e não é comparável. **A primeira fatia da COBERTURA FEITA a 2026-10-08: 160 de 272 — 58,8 %** (33 folhas novas em 37 checks + 1 que a bateria já exercitava e o parser não via — um `--help` numa segunda invocação do mesmo corpo de shell suprimia a primeira, corrigido no mesmo commit com dois testes verificados a chumbar sem a correcção). Entraram as folhas BARATAS: leituras puras, o ciclo dos segredos, os snapshots de volume, as oito podas (depois da limpeza, de propósito) e o `build`. **Falta ainda**: `systemcontainer` 0/6, `cluster` 1/16, `net` 8/36 e os verbos que precisam de hipervisor, de root ou de um nó remoto — essa metade não é barata e cada troço tem a sua pré-condição. O ramo `auditoria-cobertura` DESAPARECEU (medido 2026-10-06), por isso o resto é refazer, não integrar (ver `66_CONTINUITY_PLAN.md` §2) | M→G | `scripts/cli_exec_ratchet.py` no CI; número na matriz de rastreabilidade | M13 |
| F0.3 | **Laboratório que executa de facto**: runner self-hosted com KVM, userns não privilegiados, cgroup delegado (`cpu cpuset io memory pids`) e `br_netfilter`. Corre `e2e.sh` completo, `chaos.sh` e o `bench_gate.py` todas as noites; o job **falha** se um cenário saltar sem razão aceite | G | 3 corridas nocturnas seguidas com 0 `skipped` inesperados; log publicado | M13, M11 |
| F0.4 | **Imagem de convidado de teste**: qcow2 mínimo (< 100 MiB) que arranca em libvirt **e** CH, corre cloud-init, `qemu-guest-agent` e consola série, e responde num porto. Construída pelo próprio `vm build`, publicada no ghcr | M | `vm create --wait` → login por SSH → `hostname` igual ao pedido, nos dois backends | M09 |
| F0.5 | **Remedir o CRI** com `critest` v1.36 contra o motor actual; classificar as falhas restantes (bug / não aplicável / recusa) | P | `docs/cri-conformance.md` com data e motor de hoje | M02 |
| F0.6 | **Teste de upgrade de estado**: um root de estado criado pela tag N-1 é aberto pela N sem perda (containers, volumes, redes, segredos, VMs, registos `last-applied`) | M | job que instala a release anterior, cria estado, instala a nova e corre um `stack plan` sem diferenças | M04 |

### F1 — Onda 1: converter o que está implementado em provado (C1 + C2)

Funcionalidade nova: nenhuma. Só checks, cenários e asserções.

| Domínio | Itens | Critério de saída |
|---|---|---|
| Computação de VMs | `vm.destroy`, `vm.restart` (CH, libvirt); `vm.pause`/`vm.resume`/`vm.resize.cold` (CH); `vm.disks.extra`/`vm.nics.extra` (libvirt, arrancando o domínio); `vm.cpu.model`, `vm.cpu.pinning`, `vm.memory.hugepages` (lidos do domínio vivo) | cada célula `supported` com `check:` próprio |
| Convidado (via F0.4) | `vm.cloud-init`, `vm.ip.observed`, `vm.console.serial`, `vm.guest-agent` (libvirt, CH) | login real no convidado na bateria |
| Containers | `container.oom-detection` (o `scen_oom` passa a exigir `OOMKilled`); `pod.shared-ipc-uts` (ler `ipc`/`uts` dos membros) | asserção no cenário, que chumba com o código revertido |
| Protecção | `vm.backup.disk`/`vm.backup.restore` em CH e libvirt, com o ciclo destruir → repor → os dados voltaram | check do ciclo completo |
| Mobilidade | `vm.migration.cold` em CH e libvirt entre dois roots no mesmo host | check que confirma o disco no destino |
| Rede (linux) | `net.static-ip`, `net.dns`, `net.l7-proxy`, `net.vlan`, `net.capture` | um check por célula com prova pelo dataplane, não pela saída do comando |
| Firewall (linux) | `firewall.default-deny`, `firewall.source-filtering`, `firewall.egress-policy`, `firewall.workload-peer` | tráfego real bloqueado e permitido, com os contadores das regras lidos |
| Firewall (proxmox) | as quatro linhas `scope: vm` / `scope: systemcontainer` | um pacote a atravessar e a ser cortado numa VM do laboratório |
| Inventário | `provider.availability` (as sondas do host no `provider ls` com check) | check por provider |
| Acesso | `access.transport-verified`, `access.credential-in-vault` | check que recusa TLS não verificado sem a flag e confirma a credencial lida do cofre |

**Saída da F1**: métrica de célula ≥ **60 %**, sem nenhuma célula nova `supported`
que não tenha o check correspondente a chumbar com o código revertido.

### F2 — Onda 2: os caminhos de falha (N2 → N3)

Uma célula N2 diz que o caminho feliz funciona. Para produção é preciso provar o
que acontece quando falha.

| # | Item | Critério de saída | M |
|---|---|---|---|
| F2.1 | Um cenário de caos ou check de recusa por **cada célula N2** de containers, VMs, rede e armazenamento: processo morto a meio, disco cheio, holder reiniciado, provider inalcançável, credencial revogada | tabela «célula → cenário de falha» no `capability-matrix.md`; gate que exige o par | M04 |
| F2.2 | **Classes de exit code** em todos os caminhos de falha das células N3 (4 não existe, 5 conflito, 69 indisponível, 77 permissão) | check numérico por caminho | M12 |
| F2.3 | **Eventos**: toda a falha de uma célula N3 escreve um evento em `system events` com código `DX-` | check que lê o evento depois da falha provocada | M07 |
| F2.4 | **`WatchEvents` servido** no contrato de nó (fecha `provider.events` nos quatro providers) | cliente gRPC recebe o evento de um container que morre | M07 |
| F2.5 | **Operações assíncronas** para os providers remotos: handle de tarefa exposto pelo motor (generalizar o livro de tarefas do Proxmox) | `provider.async-operations` N2 no proxmox, decisão escrita para os locais | M10 |

### F3 — Onda 3: lacunas implementadas (C4), só as aprovadas

Cada item só entra depois de passar pela decisão de âmbito (§5) e, quando mexe
numa fronteira, por um ADR (`delonix-adr`).

| Domínio | Candidatos | Notas |
|---|---|---|
| Containers | `pod.shared-pid` | o campo já está no schema |
| VMs | `vm.clone` e `vm.template` (libvirt, CH); `vm.disk.resize`; `vm.hotplug` | o hotplug é o mais caro; avaliar a procura antes |
| System containers | backup e firewall a N2; imagem com partilha de layers | `exec`/logs/exit continuam fora: a API do Proxmox não os tem (ADR-0058) |
| Rede | `net.overlay.vxlan`/`encrypted` provados entre **dois nós reais**; `net.dns.records`/`authoritative` (F5c do ADR-0059); `net.apply.rollback` em linux; `net.verify.dataplane` | a prova entre dois nós precisa da F0.3 com dois hosts |
| Gateway (opnsense) | `update-in-place`, `nat.one-to-one`, `vpn`, `multi-wan`, `ipam.reservation`/`dhcp` | por ordem de procura real |
| Firewall | `firewall.logging` e `firewall.icmp-type` em linux | o logging por regra é o que falta para diagnóstico |
| Armazenamento | `storage.pools` (libvirt, CH), `storage.lvm-thin`, `storage.zfs-btrfs`; CIFS provisionado | decidir antes se o motor gere pools ou só os consome |
| Métricas | `metrics.prometheus` por provider, `host.capacity` | `host.capacity` é o que um escalonador externo precisa |

### F4 — Compatibilidade com o ecossistema

| # | Item | Critério de saída | M |
|---|---|---|---|
| F4.1 | CRI: corrigir as falhas classificadas como bug na F0.5 | `critest` ≥ 95 % das specs aplicáveis, número publicado | M02 |
| F4.2 | Compose: ordenar as 49 chaves em falta pela frequência em ficheiros reais (amostra pública de `compose.yaml`) e implementar o topo que cubra 95 % dos ficheiros da amostra | `migrate assess` sobre a amostra: % de ficheiros que correm sem recusa | M02 |
| F4.3 | Docker API: rever as 11 recusas contra o que o Testcontainers e o `docker compose` chamam | Testcontainers (Java e Go) a correr a suite própria contra `serve docker-api` | M02 |
| F4.4 | Contrato de nó: servir `GetNodeInfo`, `GetHealth`, `GetCapacity` (passo C do ADR-0042) | cliente gerado chama os três; OpenAPI regenerado | M10 |

### F5 — Pilares transversais de produção

| Pilar | Itens | Critério de saída | M |
|---|---|---|---|
| Segurança | fuzzing contínuo dos parsers de input não confiado (manifesto, Dockerfile, compose, tar OCI, linha de controlo do holder); auditoria ofensiva (`delonix-runtime-sec`) por release grande; perfis seccomp custom já provados mantidos no gate | `cargo fuzz` no laboratório nocturno, 0 crashes abertos; relatório de auditoria por release | M04 |
| Observabilidade | SLIs do próprio motor: latência de `run`, taxa de falha de `apply`, tempo de recuperação do holder | métricas expostas e um painel de referência; M08 deixa de ser `NOT_STARTED` | M07, M08 |
| Performance | `bench_gate.py` nocturno no laboratório (F0.3); orçamento de latência por verbo | regressão > 25 % deixa o job vermelho | M11 |
| Upgrade e estabilidade | F0.6 em cada release; schema dos manifestos com `schema-diff.sh` no gate | nenhuma release sem o teste de upgrade verde | M04 |
| Dívida estrutural | dividir o `spawn()` (~405 linhas) por fases com testes por fase; varrer os padrões fail-open apontados na auditoria v3.1 | `spawn` sem função > 120 linhas; lista fail-open fechada ou justificada | M01 |
| Documentação | cada célula N3 com página de utilizador e entrada no manual do contribuidor | gate que exige a página para uma célula N3 | M12 |

---

## 5. Decisões do dono (tomadas a 2026-10-02)

As oito aprovadas para implementar e validar. Cada uma que muda uma fronteira
(D2, D3, D4, D8) entra por ADR antes do código, como manda a `delonix-adr`.

| # | Decisão | Escolhido | Como fecha |
|---|---|---|---|
| D1 | Laboratório que executa de facto | **Sim** | workflow nocturno num runner self-hosted (`delonix-lab`) e um preflight que falha se o host não tiver KVM, userns, cgroup delegado e `br_netfilter`. Registar o runner é um passo do dono |
| D2 | Armazenamento | **O motor gere pools** (LVM-thin, ZFS/btrfs) | ADR + porta de storage + Kind; células `storage.pools`, `storage.lvm-thin`, `storage.zfs-btrfs` |
| D3 | Balanceamento L4 | **No motor** | ADR + dataplane nftables (vmap/numgen) + health-check; `net.lb.l4`, `net.lb.health-check` |
| D4 | IPv6 no dataplane | **Implementar** | ADR + `table inet` + IPAM v6 + DNS AAAA; `net.ipv6`; a recusa actual passa a opt-out |
| D5 | `br_netfilter` ausente | **Recusar** o isolamento que não seria imposto | erro com classe 69 antes de criar o workload, válvula explícita, check na bateria |
| D6 | `--wait` que esgota o tempo | **Sai com erro** | exit não-zero com classe própria, nota de quebra na próxima major |
| D7 | Live migration, gestão de frota, HA, replicação | **Fora de âmbito, confirmado** | células ficam com a razão escrita (ADR-0031, ADR-0010, componente externo) |
| D8 | Hotplug de VM | **Sim** | ADR + CPU/memória/disco/NIC a quente nos backends que o suportam; `vm.hotplug` |

Uma célula retirada por decisão passa a `unsupported-by-provider` com a razão,
nunca desaparece da matriz.

---

## 6. Metas e saída de cada fase

| Fase | Métrica de célula | Outras saídas |
|---|---|---|
| Hoje | 37 % (99/265) → **2026-10-10: 37,7 % (103/273)**, cabeçalho de `docs/providers/capability-matrix.md` | — |
| F0 | 37 % (não muda; muda a confiança) | laboratório a correr; CLI executada medida; CRI remedido; teste de upgrade |
| F1 | ≥ 60 % | 0 células N2 sem check a chumbar com reversão |
| F2 | ≥ 60 % em N3 nos domínios Containers, VMs, Protecção | `WatchEvents` servido; eventos por falha |
| F3 | ≥ 85 % N2 sobre o denominador revisto pela §5 | cada item novo com ADR ou nota de decisão |
| F4 | CRI ≥ 95 %; Compose ≥ 95 % da amostra | Testcontainers verde |
| F5 | **100 % das células aplicáveis em N3; domínios em N4** | release com o laboratório nocturno verde 14 dias seguidos |

---

## 7. Governação

- **Uma tarefa = um worktree = um PR**, segundo o `CLAUDE.md` do workspace.
- **A matriz de rastreabilidade é actualizada no mesmo commit** que muda o
  estado. Este plano também: cada fase fechada actualiza a §2 com a data e o
  commit da medição.
- **Revisão por fase** com o agente `revisor` (correcção) e, a partir da F2, com
  a skill `delonix-producao` (raio de dano e direcção da falha).
- **Donos por domínio** (skills do repo): containers `delonix-runtime-container`,
  VMs `delonix-runtime-vm` + `delonix-provider-lifecycle`, rede
  `delonix-runtime-network`, IaC `delonix-runtime-stack`, imagens
  `delonix-runtime-image`, testes `delonix-testing`, carga `delonix-carga`,
  segurança `delonix-runtime-sec`.
- **Nada do consumidor entra no motor.** Um item motivado por um consumidor
  escreve-se como capacidade do motor, no vocabulário dos seus Kinds, ou não
  entra (`AGENTS.md`, «O que o motor não conhece»).
