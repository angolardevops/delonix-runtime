# ADR-0066: Balanceamento L4 no motor — um VIP por `Service`, DNAT nftables no holder, saúde pelo supervisor que já existe

- **Estado:** Proposto (2026-10-02). Nada implementado; a evidência é o spike da secção
  «Medições», corrido num namespace descartável sem privilégio.
- **Data:** 2026-10-02
- **Decisores:** Walter Angolar
- **Relaciona-se com:** ADR-0032 (estende-o: o VIP que ele adiou), plano de maturidade
  (`docs/discovery/65_PLANO_MATURIDADE.md`, decisão **D3** aprovada: «balanceamento L4 no
  motor»; e **D5**, recusar o que o `br_netfilter` não impõe), células `net.lb.l4` e
  `net.lb.health-check` da matriz de capacidades (hoje `not-implemented` no provider `linux`),
  guarda-rios 1 (daemonless), 5 (spike antes de privilégio) e 6 (sem falha silenciosa) da
  `delonix-adr`.

## Contexto

O ADR-0032 deu ao `kind: Service` um conjunto de backends resolvido por DNS (vários registos
`A`, ordem rodada) e deixou o VIP para quando houvesse uma necessidade concreta: uma ligação
longa que o DNS não reequilibra, ou um cliente que guarda o endereço. A D3 do plano de
maturidade é essa decisão: o dono aprovou o balanceamento L4 **no motor**, e não num
componente externo. As restrições que não mudam:

- **Daemonless.** Não há processo residente por omissão; um daemon novo precisa de ADR próprio
  com a evidência do que a alternativa não resolve. O holder (pin + plano de controlo) e o slirp
  já são infra persistente, e só existem enquanto há trabalho de rede.
- **Rootless.** O dataplane vive no netns do holder (`unshare --user --net`), só nftables, sem
  `CAP_NET_ADMIN` no host.
- **Sem consumidor.** O motor não sabe quem lhe pede um VIP.

### O que já existe (lido no código, `origin/main` `b269c46e`)

1. **Um par `lbset`/`lbclear` sem um único chamador** — `crates/adapters/delonix-sdn/src/infra.rs`:
   `do_lbset`/`do_lbclear` (verbos do socket de controlo) e as funções públicas
   `set_service_lb`, `set_service_lb_algo`, `clear_service_lb`. `grep` no workspace: zero
   chamadores. Três defeitos, todos **por leitura**:
   - **Não podem funcionar com o VIP que o próprio crate calcula.** `do_lbset` recusa um VIP
     fora de `is_ingress_ip` (o espaço de workloads, `10.200`–`10.254`), e o
     `delonix_net_rules::service_vip` devolve sempre `10.90.a.b` — fora desse espaço *de
     propósito*, diz o doc-comment dele, porque um VIP dentro da subrede seria entregue
     directamente. O par só aceita o VIP que não serve.
   - **Não é atómico.** `do_lbset` chama `do_lbclear` (um `nft list` + um `nft delete` por
     handle) e depois um `nft add rule` noutra invocação: entre as duas o VIP não tem regra, e o
     tráfego segue a rota por omissão do holder (o `tap0`, para fora).
   - **`numgen inc` por omissão** — medido abaixo (E10), um contador por regra recomeça em 0 a
     cada reescrita, e com reescritas frequentes enviesa para os primeiros índices.
2. **`service_vip` (hash FNV de 16 bits em `10.90.0.0/16`)** — sem registo, logo sem detecção
   de colisão. Com *k* serviços a probabilidade de colisão é ≈ 1 − e^(−k²/2·65536): **7 % com
   100 serviços, 50 % com 300**. Dois serviços com o mesmo VIP trocam tráfego em silêncio.
3. **O índice DNS** (`build_dns_index`) já resolve o selector de cada `Service` contra os
   containers vivos (mesma namespace, `matches_labels`), dentro do processo de controlo do
   holder, com TTL de 2 s.
4. **O monitor de saúde** (`health_monitor_loop`, `bins/delonix-runtime-bin/src/cmd/container.rs`)
   corre no supervisor que todo o `run -d` já tem, executa o `--health-cmd` (ou o `HEALTHCHECK`
   da imagem) dentro do container, grava `health_state` no registo com `Store::update`, e emite
   o evento `container/health_status` **só nas transições**. O próprio comentário regista a
   decisão: «this engine is daemonless, so there is nobody resident to poll. The supervisor is
   the honest answer».
5. **O IPAM** (`ipam::allocate`/`reserve`/`release`, por prefixo, sob `IpamLock`) e o seu
   ceifador `reap_orphan_leases`, cuja vivacidade sai de `prune::lease_owners`. Um lease
   chaveado por algo que não é um container é reclamado se `lease_owners` não o conhecer — a
   lição já paga com os pods (`pod-<nome>`).

## Medições (spike, 2026-10-02)

Kernel 7.0.0-34, nftables 1.0.9. Tudo dentro de
`unshare --user --map-root-user --net --mount --pid --fork --mount-proc` (o holder é exactamente
isto), sem tocar no host. Topologia: `br0` `10.233.0.1/16` com três backends (`b1..b3`,
`10.233.0.11-13`, um servidor TCP em Python que responde `<nome> <ip-do-par>` a cada ligação e a
cada linha), um cliente `c1` **na mesma bridge** (`10.233.0.50`), um cliente `c2` noutra bridge
(`br1`, `10.234.0.50`, encaminhado pelo «holder»), e o VIP `10.90.0.10:80`. Os scripts
(`setup.sh`, `backend.py`, `client.py`, `e0`–`e3.sh`) ficaram no scratchpad da sessão; o
essencial está aqui.

A regra medida:

```
table ip lb {
  chain pre { type nat hook prerouting priority -100;
    ip daddr 10.90.0.10 tcp dport 80 dnat ip to numgen random mod 3 map { 0 : 10.233.0.11 . 80, 1 : 10.233.0.12 . 80, 2 : 10.233.0.13 . 80 }
  }
  chain out { type nat hook output priority -100;   # o mesmo, para clientes do próprio holder
    ...
  }
}
```

| # | O quê | Resultado |
|---|---|---|
| E0 | O `nft` monta a regra dentro do userns | `rc=0`. O `nft_numgen` **não estava carregado** no host e o kernel carregou-o (autoload) a pedido do userns — ficou carregado; foi o único efeito fora do namespace. O `jhash` (`nft_hash`) **não foi medido de propósito**, para não carregar outro módulo |
| E1 | `numgen random`, `c2`, 5 × 300 ligações | `99/97/104`, `114/86/100`, `112/100/88`, `106/101/93`, `102/88/110` (desvio máximo 14 % de 100) |
| E1 | `numgen random`, 3000 ligações | `996/975/1029` (±2,5 %) |
| E2 | `numgen inc`, 300 ligações | `100/100/100` |
| E3 | Hairpin: cliente `c1` na mesma bridge, `bridge-nf-call-iptables=1` | `20/20/20`, e o backend vê o **IP real do cliente** (`10.233.0.50`) |
| E3 | O mesmo com `bridge-nf-call-iptables=0` | **60/60 `TimeoutError`** (60 s): o backend responde directamente pela bridge com o seu IP, o cliente esperava o VIP |
| E3 | `=0` + `iifname br0 oifname br0 ct status dnat masquerade` | `20/20/20`, mas o backend vê **`10.233.0.1`** (o gateway) — o IP do cliente perde-se |
| E3 | O sysctl é por netns e gravável pelo root do userns | `sysctl -w net.bridge.bridge-nf-call-iptables=0` funcionou; num netns novo vale `1` |
| E4 | Cliente no próprio holder (cadeia `nat output`), sem rota para o VIP | `Errno 101 Network is unreachable` (a decisão de rota precede o NAT de saída) |
| E4 | Com rota por omissão (o holder real tem uma, pelo `tap0`) | `2/2/2`, origem `10.233.0.1` |
| E5 | Um `drop` em `forward` (prioridade −5) sobre `daddr` do b1 | b1 recebe **0**; 20 de 60 ligações dão timeout, b2/b3 respondem 20/20; contador da regra `packets 20` — o filtro por container vê o backend real, **depois** do DNAT |
| E6 | Backend b2 morto mas ainda no mapa, 300 ligações | `ConnectionRefused` 100, b1 100, b3 100 — um terço falha |
| E7 | Ejectar b2: reescrever a regra num só `nft -f` | 18 ms; 300 ligações → `150/150`, zero erros. Cinco reescritas: 26/19/25/23/24 ms (inclui arrancar o processo `nft`) |
| E8 | Uma ligação longa cujo backend sai do mapa | Aterrou no b1; b1 tirado do mapa; as 5 linhas seguintes **na mesma ligação** responderam `b1` (o conntrack guarda o DNAT); 200 ligações novas → `b2:100, b3:100` |
| E9 | Porta publicada: um `tap0` simulado (`10.0.2.100`) com `ip daddr 10.0.2.100 tcp dport 8080 dnat … map` ao mesmo conjunto | `30/30/30` |
| E10 | 50 reescritas atómicas (`flush chain svc` + `add rule`, um `nft -f`) **durante** 3000 ligações, a alternar 3 e 2 backends, com `numgen inc` | **zero erros**; `b1:1266, b2:486, b3:1248` — o enviesamento do `inc` com reescritas |
| E11 | Uma transacção a reescrever 200 serviços × 3 backends (30 KB de script) | 44/47/48 ms; 200 regras na cadeia |
| E12 | Uma transacção com um passo inválido | `rc=1` e nada aplicado (a regra válida do mesmo script não ficou) |

Lições do spike que entram nas decisões: (a) o mecanismo funciona inteiro sem privilégio no
host; (b) o hairpin **depende** do `br_netfilter` ou perde o IP de origem; (c) a reescrita
completa e atómica custa dezenas de milissegundos, por isso não precisa de ser incremental;
(d) `numgen inc` e reescritas frequentes não combinam; (e) um backend morto no mapa custa 1/N
das ligações, e é por isso que a saúde é parte do L4 e não um extra.

## Decisão

### D1 — Não é um Kind novo: `kind: Service` ganha `spec.type`

```yaml
apiVersion: networking.delonix.io/v1alpha1
kind: Service
metadata: { name: web, namespace: teamA }
spec:
  selector: { matchLabels: { app: web } }
  port: 8080            # porta do container E do VIP (v1)
  type: VirtualIP       # DNS (omissão) | VirtualIP
  vip: 10.90.4.20       # opcional; sem ele o IPAM escolhe
  publish: [18080]      # opcional; portas do host que levam ao VIP (D6)
```

- `type: DNS` (omissão) é o ADR-0032 **byte a byte**: nenhum manifesto existente muda.
- `type: VirtualIP`: o nome `<svc>.<ns>.delonix.internal` passa a resolver para **um** `A`, o
  VIP, com a mesma regra de namespace do ADR-0032.
- **Porquê não um Kind `LoadBalancer`:** o selector, a namespace, o nome DNS e a posse por
  `delonix.io/stack` seriam os mesmos; dois Kinds com o mesmo selector são duas leituras do
  mesmo `matchLabels` a divergir — o que o ADR-0032 já recusou para o `FirewallPolicy`. O que o
  `type` muda é **como** o conjunto é servido, não **qual** é.
- No reconciliador, `type`, `vip`, `publish`, `selector` e `port` são campos **quentes**: mudar
  qualquer um reescreve o dataplane e o registo, sem estado a perder. Mudar o `vip` di-lo em voz
  alta (quem guardou o endereço antigo deixa de chegar).

### D2 — O VIP vem do IPAM, de um pool próprio, e fica no registo

- Pool **`10.90.0.0/16`**, fora do espaço de workloads (é o que faz o tráfego passar pelo
  gateway, onde está o DNAT — E4/E5). Lease chaveado `svc:<namespace>/<name>`, guardado em
  `ServiceDef.vip`, libertado no `delete`/`--prune`/`destroy`.
- `spec.vip` explícito é **reservado** (`ipam::reserve`) ou recusado: fora do pool → DX novo de
  classe *invalid*; já usado por outro serviço → `Conflict` (exit 5).
- `prune::lease_owners` passa a contar os serviços declarados como donos — sem isso o ceifador
  do IPAM reclama o VIP e entrega-o ao serviço seguinte (a armadilha dos pods).
- Uma rede declarada (`network create`, `cidr=`) que se sobreponha ao pool é recusada, e o pool
  recusa-se a nascer sobre uma rede existente.
- **Rejeitado:** o VIP derivado por hash (`service_vip`) — 50 % de colisão aos 300 serviços, sem
  registo onde a ver.

### D3 — Dataplane: uma cadeia `svc`, reescrita inteira e atómica

- Ruleset base do holder ganha `chain svc` (nat, sem hook), um `jump svc` no `pre` e uma cadeia
  `out` (`type nat hook output priority -100; jump svc`) para os clientes do próprio holder (o
  proxy L7 pode ter um `Service` como backend). O holder já tem rota por omissão pelo `tap0`, o
  que o E4 mostrou ser necessário.
- Por VIP e porta: `ip daddr <vip> tcp dport <port> dnat ip to numgen random mod <N> map { … }`.
  **`numgen random` e não `inc`**: o `random` não tem estado, e o `inc` recomeça a cada reescrita
  (E10). A distribuição medida (E1) é a de um gerador uniforme.
- **Conjunto vazio → `reject with tcp reset`** na mesma cadeia, para o cliente falhar já e não
  ao fim de um timeout. **Por medir.**
- **Porta não servida de um VIP não sai pelo `tap0`.** Um VIP não está em interface nenhuma:
  sem uma regra, um pacote para `10.90.x.y:<outra porta>` seguiria a rota por omissão e o
  masquerade levava-o ao host. Fica um `ip daddr 10.90.0.0/16 reject` no `fwguard` (filtro,
  depois do NAT, logo só apanha o que nenhum DNAT reescreveu). **Por medir.**
- **A reescrita é sempre total**: `flush chain ip dlxing svc` + todas as regras num só
  `nft -f` (E10: zero erros com tráfego; E12: um script inválido não aplica nada). Não há
  operações incrementais sobre mapas, e por isso não há a classe de bug de ordem do `@netpair`.
  Custo: ~50 ms para 200 × 3 (E11).
- TCP e UDP: o `port` do v1 é TCP; `protocol: udp` entra com o mesmo mecanismo quando houver um
  uso concreto (o mapa é o mesmo, muda o `dport`).

### D4 — Alcance e isolamento não mudam

O DNAT corre em `prerouting`; as cadeias por container (`fwout` −6, `fwcont` −5) correm no
`forward`, **depois**, sobre o backend real (E5). Logo o isolamento de namespace, o
`NetworkPolicy`, o `Dependency` e o `NetworkAccessRule` aplicam-se como se o cliente tivesse
usado o IP do backend. Um VIP não fura nenhuma fronteira: um cliente de outra namespace
chega ao VIP e é recusado pela cadeia de cada backend, exactamente como chegaria ao IP directo.

### D5 — Hairpin: exige `br_netfilter`, e recusa sem ele

Com o cliente na mesma bridge dos backends — o caso comum — o hairpin só funciona com
`bridge-nf-call-iptables=1` (E3: 60/60 timeouts sem ele). A alternativa, um `masquerade` do
tráfego DNAT que volta à mesma bridge, funciona mas faz o backend ver o gateway e não o cliente
(E3), o que tira à aplicação o IP de origem. Decisão: **sem masquerade**; o `type: VirtualIP` é
recusado com classe 69 (`EX_UNAVAILABLE`) quando o holder não tem
`/proc/sys/net/bridge/bridge-nf-call-iptables` a `1` — a mesma pré-condição e a mesma classe da
D5 do plano. O holder põe-no a `1` no seu netns (é gravável pelo root do userns, E3); só recusa
quando o módulo não existe no host.

### D6 — Publicar o VIP no host: o caminho do `-p` que já existe

`spec.publish: [<hostPort>]` usa o `slirp_add_hostfwd` de sempre (bind por omissão a
`127.0.0.1`, `DELONIX_PUBLISH_ADDR` para alargar) e põe na cadeia `svc` a regra
`ip daddr 10.0.2.100 tcp dport <hostPort> dnat ip to numgen random mod N map { … }` — o mesmo
conjunto, outra porta de entrada (E9). **Não há duplo DNAT** (para o VIP e depois para o
backend): um DNAT em `prerouting` é terminal, por isso a regra aponta directamente aos backends.
A origem chega intacta para clientes encaminháveis e como `10.0.2.2` para os de loopback (a
regra já documentada no AGENTS.md).

### D7 — O conjunto mantém-se actual sem daemon

- **Uma função, dois consumidores:** `service_backends(def, containers)` passa a ser a única
  avaliação do selector (mesma namespace, `matches_labels`, workload vivo por pid + `starttime`,
  com IP numa rede, porta de saúde da D8). O índice DNS e a reescrita do dataplane usam-na; não
  há segunda leitura do `matchLabels`.
- **Quem reescreve:** o processo de controlo do holder, num verbo novo `svcsync` — o mesmo
  processo que já constrói o índice DNS a partir dos mesmos registos. Só existe enquanto a infra
  existe, que é exactamente quando há VIPs para servir.
- **Quando:** (1) `apply`/`delete` de um `Service` (CLI → `svcsync`); (2) no fim do `do_attach`
  e do `do_detach` (já correm dentro do holder — chamada directa); (3) quando o supervisor grava
  a morte de um container (`Crashed`/`Exited`), porque uma morte sem `detach` deixaria o IP no
  mapa; (4) nas transições de saúde (D8); (5) no arranque do plano de controlo — cobre o
  reinício do controlo (o pin guarda o ruleset, mas uma mudança feita enquanto o controlo estava
  em baixo só se vê aqui) e a reconstrução completa.
- **Sem reconciliação periódica.** Uma mudança que nenhum dos cinco caminhos vê (um registo
  editado à mão) fica até ao próximo evento. Fica escrito, não escondido.
- Um container em primeiro plano não tem supervisor: a morte dele é vista pelo `detach` do
  próprio `run`, que é o caminho (2).

### D8 — Saúde: o monitor do supervisor decide quem está em rotação

Opções avaliadas (o custo de cada uma é o que decide):

| Opção | Custo | Veredicto |
|---|---|---|
| **A. O monitor de saúde do supervisor** (`--health-cmd` / `HEALTHCHECK`), já existente | Zero processos novos; a sonda corre dentro do container e mede o que o autor da aplicação definiu; a transição já é detectada (emite `health_status`). Latência de ejecção = `interval × retries` (omissões do Docker: 30 s × 3 = 90 s). Só cobre containers com monitor (`run -d`, que é o caminho do `stack apply`) e imagens com shell | **Escolhida** |
| B. Sonda TCP numa thread do processo de controlo do holder | Sem processo novo, mas um temporizador residente no controlo, e estado perdido em cada reinício do controlo; testa só «a porta abre», não «a aplicação está pronta» | Fase 4, condicional (imagens sem shell, *distroless*) |
| C. Timer de systemd por serviço | Depende de uma sessão systemd de utilizador; um processo por tick; granularidade grosseira por omissão (`AccuracySec=1min`); rootless e root com caminhos diferentes | Rejeitada |
| D. Sondar só nos eventos (`stack`/`container`) | Nenhum processo; um backend pendurado nunca é ejectado | Rejeitada como mecanismo único (é o D7) |
| E. Um processo supervisor por `Service` | Um daemon por serviço, com ciclo de vida, guarda de identidade de pid e reaping próprios | Rejeitada: é o que o guarda-rio 1 proíbe sem necessidade provada, e A cobre o caso |
| F. Nada | O E6 mede o custo: 1/N das ligações recusadas enquanto um backend morto está no mapa | Rejeitada |

Regras da A:

- Container **com** `HealthConfig`: em rotação só quando `health_state.health == Healthy`.
  `Starting` fica **fora** (prontidão, não vivacidade): um backend que ainda arranca não recebe
  tráfego.
- Container **sem** monitor: em rotação enquanto vivo (o ADR-0032 inalterado). O `get services`
  diz quantos backends não têm sonda.
- Na transição, o `health_monitor_loop` envia `svcsync` logo a seguir a emitir o evento
  `health_status` (best-effort: holder em baixo = nada a reescrever).
- **As ligações já abertas a um backend ejectado continuam** (E8): o conntrack guarda o DNAT, e
  um pedido a meio acaba. Não se apaga o conntrack na ejecção por saúde. Na morte do container
  não há nada a apagar (o outro lado já não existe).
- **Todos os backends não saudáveis → conjunto vazio → `reject`** (D3), e o `get services` di-lo.
  Não se cai para «todos, mesmo os doentes»; seria uma degradação silenciosa.

### D9 — O par `lbset`/`lbclear` e o `service_vip` saem

`do_lbset`, `do_lbclear`, os verbos `lbset`/`lbclear` do socket, `set_service_lb`,
`set_service_lb_algo`, `clear_service_lb`, o `delonix_net_rules::service_vip` e o
`Error::InvalidLbSpec` saem na fase 1. Não têm chamadores, o par não aceita o VIP que o hash
produz, e a reescrita não é atómica. É quebra para um utilizador **da biblioteca**
`delonix-sdn` (a mesma nota que a remoção do `Net` deixou): sobe no CHANGELOG da release.

## Alternativas consideradas

- **Ficar só com o DNS (ADR-0032).** Não reequilibra uma ligação longa nem serve um cliente
  que guarda o IP, e a D3 aprovada pede o L4.
- **IPVS.** Exige o módulo `ip_vs` e `CAP_NET_ADMIN` no namespace do utilizador inicial
  para a maior parte da configuração; **por leitura, não medido**. Não acrescenta nada que o
  `numgen` não dê para o conjunto pequeno de um nó, e mete um segundo plano de dados ao lado das
  nftables.
- **Proxy em espaço de utilizador** (alargar o proxy L7 a TCP). Copia bytes, é um processo
  residente por porta, e o IP de origem chega como o do proxy. O L7 continua a ser a resposta para
  HTTP; o L4 é do kernel.
- **eBPF/XDP.** `CAP_BPF` não existe num userns sem privilégio; o motor já degrada o `net flow`
  pela mesma razão.
- **Afinidade por `jhash ip saddr` por omissão.** O ADR propõe `sessionAffinity: ClientIP`
  como campo opcional (Fase 2b), não como omissão: muda o mapeamento sempre que N muda, e não foi
  medido neste spike (para não carregar o `nft_hash` no host).
- **Masquerade do hairpin** em vez de exigir o `br_netfilter` — rejeitado na D5 (perde o IP de
  origem, E3).

## Plano por fases

| Fase | O quê | Ficheiros |
|---|---|---|
| F1 — modelo e limpeza | `ServiceDef.{type, vip, publish}` (`#[serde(default)]`); pool de VIPs no IPAM; `lease_owners` conta os serviços; recusas de sobreposição; schema e `explain`; `hot_fields` do `Service`; saem o `lbset`/`lbclear`/`service_vip` (D9); DX novos no dicionário e `pt.po` | `crates/adapters/delonix-sdn/src/{infra.rs,ipam.rs,error.rs}`, `crates/foundation/delonix-net-rules/src/lib.rs`, `crates/foundation/delonix-model/src/codes.rs`, `bins/delonix-runtime-bin/src/cmd/{service.rs,prune.rs,schema.rs}`, `crates/contexts/delonix-stack/src/reconcile.rs`, `docs/schema/v1/delonix.json`, `data/pt.po` |
| F2 — dataplane | cadeias `svc`/`out` no ruleset base (criadas também por um `svcsync` contra um holder antigo, sem duplicar o `jump`); `service_backends` partilhada com o índice DNS; verbo `svcsync` e os cinco gatilhos da D7; DNS de um `VirtualIP`; `publish` (D6); recusa sem `br_netfilter` (D5); `reject` do vazio e da porta não servida | `crates/adapters/delonix-sdn/src/infra.rs`, `bins/delonix-runtime-bin/src/cmd/{service.rs,container.rs}`, `crates/adapters/delonix-linux/src/supervise.rs` |
| F3 — saúde | porta de saúde em `service_backends`; `svcsync` no `health_monitor_loop`; `get/describe services` com `READY/TOTAL` e quantos sem sonda | `bins/delonix-runtime-bin/src/cmd/{container.rs,service.rs}` |
| F4 — condicional | sonda TCP no controlo (opção B) — só se aparecer uma imagem sem shell que precise de ser ejectada | — |

A matriz passa `net.lb.l4` a `partial` no fim da F2 e a `supported` quando os checks abaixo
existirem e passarem; `net.lb.health-check` idem no fim da F3. A evidência citada é o título do
check, como manda o ADR-0050.

### Gates

Testes puros (sem holder): a renderização do script `nft` (vazio → `reject`; N backends → mapa
de N; `publish` → segunda regra; nomes e IPs validados antes do argv); `service_backends` (a
porta de saúde, `Starting` fora, namespace); a recusa de VIP fora do pool e duplicado; o
`lease_owners` com um lease `svc:`.

Bateria (`scripts/e2e.sh`, secção nova «kind: Service type VirtualIP», raiz isolada com os dois
roots):

- «um VIP reparte as ligações por todos os backends» — 300 ligações de um container de outra rede,
  cada backend com pelo menos 20 %;
- «um cliente na mesma rede dos backends chega ao VIP e o backend vê o IP dele» (hairpin, D5);
- «o nome do serviço resolve para o VIP»;
- «um backend removido (`container rm`) deixa de receber ligações novas» — zero ligações a ele em
  200, sem `stack apply`;
- «um backend não saudável sai de rotação e volta quando recupera» (`--health-cmd` que lê um
  ficheiro; apagar e repor o ficheiro);
- «um VIP sem backends recusa em vez de pendurar» (`ConnectionRefused` em < 1 s);
- «uma porta não servida do VIP não sai do holder»;
- «um cliente de outra namespace não passa do VIP» (D4);
- «a porta publicada leva ao mesmo conjunto»;
- «sem `br_netfilter` o `type: VirtualIP` é recusado com classe 69» (só onde o módulo falta;
  senão `SKIP` audível);
- «o pool recusa um VIP duplicado (exit 5) e fora do pool».

Caos (`scripts/chaos.sh`, cenário novo `service_lb`): três backends e um cliente em ciclo
contínuo; (1) `kill -9` a um backend com `--restart no` — o cliente vê no máximo as ligações até
ao supervisor gravar a morte, e depois zero erros; (2) sonda de saúde a falhar num segundo
backend — ejectado em ≤ `interval × retries` + 2 s; (3) `kill -9` ao plano de controlo do
holder — o pin guarda o ruleset, o tráfego **não pára**, e o conjunto reescrito no reinício é
igual ao anterior; (4) `netns down`/`up` (morte do pin) — o VIP volta com o mesmo endereço
(vem do registo, não de um hash). O cenário compara as contagens por backend e não só «o cliente
recebeu respostas», porque um VIP que só servisse um backend também responderia.

## Consequências

- O `Service` passa a ter duas formas de serviço sob o mesmo Kind; a omissão não muda.
- O holder ganha uma responsabilidade nova (reescrever a cadeia `svc`), sem processo novo.
- A saúde depende do monitor do supervisor, logo de um `--health-cmd` ou de um `HEALTHCHECK`
  e de uma imagem com shell. Um backend sem sonda é servido enquanto vive, e isso é dito.
- A ejecção é tão rápida quanto o `interval × retries` do container: com as omissões do Docker,
  90 s. A documentação do `Service` tem de recomendar valores curtos.
- O pool `10.90.0.0/16` passa a ser reservado; uma rede do utilizador nessa gama é recusada.
- Sai uma API pública da biblioteca `delonix-sdn` (D9).
- IPv6 fica de fora (D4 do plano trata a `table inet`); o VIP é IPv4 como o resto do dataplane.

## Questões para o dono

1. **O pool `10.90.0.0/16`** é o certo, ou deve ser configurável (por exemplo
   `DELONIX_SERVICE_VIP_CIDR`)? Um host cuja LAN use `10.90/16` perde o acesso dos containers a
   esses endereços **só** quando um deles for VIP.
2. **`port` único no v1**, ou já `ports: [{port, targetPort, protocol}]` à Kubernetes? O ADR
   propõe o único e uma fase própria para a lista.
3. **`Starting` fora de rotação** — confirma a semântica de prontidão (um backend acabado de
   arrancar não recebe tráfego até à primeira sonda boa)?
4. **D9 — remover a API `set_service_lb*`/`service_vip`** da biblioteca: aceitável na próxima
   release, ou fica uma release com `#[deprecated]`?
5. **A sonda TCP (opção B)** fica condicional, ou entra já na F3 para cobrir imagens sem shell?
