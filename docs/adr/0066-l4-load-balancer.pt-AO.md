# ADR-0066: Balanceamento L4 no motor — um VIP por `Service`, DNAT nftables no holder, backends admitidos por prontidão

> **Cópia de revisão interna, em português (pt-AO).** O ADR canónico é
> `0066-l4-load-balancer.md`, em inglês; as decisões são as mesmas nos dois.

- **Estado:** Proposed (2026-10-02). Nada implementado; a evidência é o spike da secção
  «Medições», corrido num namespace descartável sem privilégio. As respostas do dono de
  2026-10-02 ficam registadas como decididas (D2, D8, D9, «Decisões do dono»).
- **Data:** 2026-10-02
- **Decisores:** Walter Angolar
- **Relaciona-se com:** ADR-0032 (estende-o: o VIP que ele adiou), plano de maturidade
  (`docs/discovery/65_PLANO_MATURIDADE.md`, decisão **D3** aprovada: «balanceamento L4 no
  motor»; e **D5**, recusar o que o `br_netfilter` não impõe), células `net.lb.l4` e
  `net.lb.health-check` da matriz de capacidades (hoje `not-implemented` no provider `linux`),
  guarda-rios 1 (daemonless), 5 (spike antes de privilégio) e 6 (sem falha silenciosa) da
  `delonix-adr`.

## Contexto

O ADR-0032 deu ao `kind: Service` um conjunto de backends resolvido por DNS (vários registos `A`,
ordem rodada) e deixou o VIP para quando houvesse uma necessidade concreta: uma ligação longa que
o DNS não reequilibra, ou um cliente que guarda o endereço. A D3 do plano é essa necessidade: o
dono aprovou o balanceamento L4 **no motor**, e não num componente externo. O que não muda:

- **Daemonless.** Nenhum processo residente por omissão; um daemon novo precisa de ADR próprio com
  a evidência do que a alternativa não resolve. O holder (pin + plano de controlo) e o slirp já são
  infra persistente, e só existem enquanto há trabalho de rede.
- **Rootless.** O dataplane vive no netns do holder (`unshare --user --net`), só nftables, sem
  `CAP_NET_ADMIN` no host.
- **Sem consumidor.** O motor não sabe quem lhe pede um VIP.

### O que já existe (lido no código, `origin/main` `b269c46e`)

1. **Um par `lbset`/`lbclear` sem chamador** — `crates/adapters/delonix-sdn/src/infra.rs`:
   `do_lbset`/`do_lbclear` (verbos do socket de controlo) e as funções públicas `set_service_lb`,
   `set_service_lb_algo`, `clear_service_lb`. `grep` no workspace: zero chamadores. Três
   defeitos, todos **por leitura**:
   - **Não funcionam com o VIP que o próprio crate calcula.** `do_lbset` recusa um VIP fora de
     `is_ingress_ip` (o espaço de workloads, `10.200`–`10.254`), e o
     `delonix_net_rules::service_vip` devolve sempre `10.90.a.b` — fora desse espaço *de
     propósito*, diz o doc-comment, porque um VIP dentro da subrede seria entregue directamente.
     O par só aceita o VIP que não serve.
   - **Não é atómico.** `do_lbset` chama `do_lbclear` (um `nft list` + um `nft delete` por handle)
     e depois um `nft add rule` noutra invocação: entre as duas o VIP fica sem regra, e o tráfego
     segue a rota por omissão do holder (o `tap0`, para fora).
   - **`numgen inc` por omissão** — medido abaixo (E10): um contador por regra recomeça em 0 a cada
     reescrita, e com reescritas frequentes enviesa para os primeiros índices.
2. **`service_vip` (hash FNV de 16 bits em `10.90.0.0/16`)** — sem registo, logo sem detecção de
   colisão. Com *k* serviços a probabilidade de colisão é ≈ 1 − e^(−k²/2·65536): **7 % com 100
   serviços, 50 % com 300**. Dois serviços com o mesmo VIP trocam tráfego em silêncio.
3. **O índice DNS** (`build_dns_index`) já resolve o selector de cada `Service` contra os containers
   vivos (mesma namespace, `matches_labels`), no processo de controlo do holder, com TTL de 2 s.
4. **O monitor de saúde** (`health_monitor_loop`, `bins/delonix-runtime-bin/src/cmd/container.rs`)
   corre no supervisor que todo o `run -d` já tem, executa o `--health-cmd` (ou o `HEALTHCHECK` da
   imagem) dentro do container, grava `health_state` com `Store::update`, e emite o evento
   `container/health_status` **só nas transições**.
5. **O IPAM** (`ipam::allocate`/`reserve`/`release`, por prefixo, sob `IpamLock`) e o ceifador
   `reap_orphan_leases`, cuja vivacidade sai de `prune::lease_owners`. Um lease chaveado por algo
   que não é um container é reclamado se o `lease_owners` não o conhecer — a lição já paga com os
   pods (`pod-<nome>`).

## Medições (spike, 2026-10-02)

Kernel 7.0.0-34, nftables 1.0.9. Tudo dentro de
`unshare --user --map-root-user --net --mount --pid --fork --mount-proc` (que é exactamente o
holder), sem tocar no host. Topologia: `br0` `10.233.0.1/16` com três backends (`b1..b3`,
`10.233.0.11-13`, um servidor TCP em Python que responde `<nome> <ip-do-par>` por ligação e por
linha), um cliente `c1` **na mesma bridge** (`10.233.0.50`), um cliente `c2` noutra bridge
(`br1`, `10.234.0.50`, encaminhado pelo «holder»), e o VIP `10.90.0.10:80` (endereço de
laboratório; ver D2). Os scripts (`setup.sh`, `backend.py`, `client.py`, `probe.py`, `e0`–`e4.sh`)
ficaram no scratchpad da sessão; o essencial está aqui.

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
| E3 | Hairpin: `c1` na bridge dos backends, `bridge-nf-call-iptables=1` | `20/20/20`, e o backend vê o **IP real do cliente** (`10.233.0.50`) |
| E3 | O mesmo com `bridge-nf-call-iptables=0` | **60/60 `TimeoutError`** (60 s): o backend responde directamente pela bridge com o seu IP, o cliente esperava o VIP |
| E3 | `=0` + `iifname br0 oifname br0 ct status dnat masquerade` | `20/20/20`, mas o backend vê **`10.233.0.1`** (o gateway) — o IP do cliente perde-se |
| E3 | O sysctl é por netns e gravável pelo root do userns | `sysctl -w net.bridge.bridge-nf-call-iptables=0` funcionou; num netns novo vale `1` |
| E4 | Cliente no próprio holder (cadeia `nat output`), sem rota para o VIP | `Errno 101 Network is unreachable` (a decisão de rota precede o NAT de saída) |
| E4 | Com rota por omissão (o holder real tem uma, pelo `tap0`) | `2/2/2`, origem `10.233.0.1` |
| E5 | Um `drop` em `forward` (prioridade −5) sobre o `daddr` do b1 | b1 recebe **0**; 20 de 60 ligações dão timeout, b2/b3 respondem 20/20; contador `packets 20` — o filtro por container vê o backend real, **depois** do DNAT |
| E6 | Backend b2 morto mas ainda no mapa, 300 ligações | `ConnectionRefused` 100, b1 100, b3 100 — um terço falha |
| E7 | Ejectar b2: reescrever a regra num só `nft -f` | 18 ms; 300 ligações → `150/150`, zero erros. Cinco reescritas: 26/19/25/23/24 ms (inclui arrancar o `nft`) |
| E8 | Uma ligação longa cujo backend sai do mapa | Aterrou no b1; b1 tirado; as 5 linhas seguintes **na mesma ligação** responderam `b1` (o conntrack guarda o DNAT); 200 ligações novas → `b2:100, b3:100` |
| E9 | Porta publicada: um `tap0` simulado (`10.0.2.100`) com `ip daddr 10.0.2.100 tcp dport 8080 dnat … map` ao mesmo conjunto | `30/30/30` |
| E10 | 50 reescritas atómicas (`flush chain svc` + `add rule`, um `nft -f`) **durante** 3000 ligações, a alternar 3 e 2 backends, com `numgen inc` | **zero erros**; `b1:1266, b2:486, b3:1248` — o enviesamento do `inc` com reescritas |
| E11 | Uma transacção a reescrever 200 serviços × 3 backends (30 KB) | 44/47/48 ms; 200 regras na cadeia |
| E12 | Uma transacção com um passo inválido | `rc=1` e nada aplicado (a regra válida do mesmo script não ficou) |
| E13 | Sonda TCP de prontidão a partir do netns do holder, timeout 500 ms, 200 rondas | backend à escuta: 200 × `ready`, < 0,1 ms cada; processo morto: 200 × `ECONNREFUSED`, < 0,1 ms; porta descartada por um filtro no netns do backend: `timeout`, 500 ms cada |
| E14 | VIP **sem** regra de DNAT (nenhum backend pronto), filtro `ip daddr <pool> meta l4proto tcp reject with tcp reset` + `reject with icmp type port-unreachable` em forward −20 | TCP: 5/5 `ConnectionRefused` (a primeira em 166 ms, com a resolução de vizinho); UDP: `ConnectionRefused` em 0 ms |
| E14 | O mesmo sem o filtro, com rota por omissão no holder (um dummy no lugar do `tap0`) | cliente `TimeoutError` ao fim de 3 s; contador na rota por omissão: **3 SYN saíram do holder** |
| E15 | Uma porta servida do VIP ao lado do filtro de recusa | porta 80 (com DNAT): 3/3 respondidas; porta 81 (não servida): 3/3 `ConnectionRefused` |

O que o spike traz para as decisões: (a) o mecanismo funciona inteiro sem privilégio no host;
(b) o hairpin **depende** do `br_netfilter` ou perde o IP de origem; (c) a reescrita completa e
atómica custa dezenas de milissegundos, por isso não precisa de ser incremental; (d) `numgen inc` e
reescritas frequentes não combinam; (e) um backend morto no mapa custa 1/N das ligações, e é por
isso que a prontidão é parte do L4 e não um extra; (f) um VIP sem nada atrás tem de ser recusado
explicitamente, senão o tráfego sai do holder pela rota por omissão.

## Decisões do dono (2026-10-02)

Registadas como decididas, e aplicadas abaixo:

1. **O pool de VIPs é configurável.** `10.90.0.0/16` aparece só como exemplo de laboratório,
   nunca como reserva universal automática. A configuração e a validação são a D2.
2. **`Starting` fica fora de rotação.** A admissão depende da prontidão; estar vivo não é estar
   pronto (D8).
3. **Sem sonda definida → um critério mínimo explícito**, nunca «saudável porque o processo
   arrancou». A sonda TCP de prontidão passa de «fase 4 condicional» para o plano principal (D8).
4. **Sem backend pronto → indisponibilidade previsível**: TCP reset / ICMP port-unreachable, nunca
   encaminhar para containers ainda a arrancar (D3).

## Decisão

### D1 — Não é um Kind novo: `kind: Service` ganha `spec.type`

```yaml
apiVersion: networking.delonix.io/v1alpha1
kind: Service
metadata: { name: web, namespace: teamA }
spec:
  selector: { matchLabels: { app: web } }
  port: 8080            # porta do container E do VIP (v1; questão aberta 1)
  type: VirtualIP       # DNS (omissão) | VirtualIP
  vip: 10.90.4.20       # opcional; tem de estar no pool configurado; sem ele o IPAM escolhe
  publish: [18080]      # opcional; portas do host que levam ao VIP (D6)
  readiness:            # opcional; a verificação TCP está sempre ligada, estes são os seus botões (D8)
    tcp: { intervalSeconds: 2, timeoutMilliseconds: 500, successThreshold: 1, failureThreshold: 2 }
```

- `type: DNS` (omissão) é o ADR-0032 **byte a byte**: nenhum manifesto existente muda.
- `type: VirtualIP`: o nome `<svc>.<ns>.delonix.internal` resolve para **um** `A`, o VIP, com a
  regra de namespace do ADR-0032.
- **Porquê não um Kind `LoadBalancer`:** selector, namespace, nome DNS e posse por
  `delonix.io/stack` seriam os mesmos; dois Kinds com o mesmo selector são duas leituras do mesmo
  `matchLabels` a divergir — o que o ADR-0032 já recusou para o `FirewallPolicy`. O `type` muda
  **como** o conjunto é servido, não **qual** é.
- No reconciliador, `type`, `vip`, `publish`, `selector`, `port` e `readiness` são campos
  **quentes**: mudar qualquer um reescreve o dataplane e o registo, sem estado a perder. Mudar o
  `vip` di-lo em voz alta (quem guardou o endereço antigo deixa de chegar).

### D2 — O VIP vem do IPAM, de um pool que o operador configura

**Sem pool por omissão.** Num nó sem pool, `type: VirtualIP` é recusado com classe 69
(`EX_UNAVAILABLE`, pré-condição do host), e a mensagem nomeia o comando que o define. Uma reserva
universal tiraria em silêncio uma gama a todos os hosts onde o motor corre.

**Onde se configura** — uma definição do nó, não um campo de cada `Service` (todos os Services do
nó tiram do mesmo espaço de endereços):

- `delonix network vip-pool set <cidr>[,<cidr>...]` / `clear` / `show`, guardado em
  `<root>/ingress/vip-pool` (um CIDR por linha, escrita atómica) — a mesma forma do
  `<root>/vm-default-backend` do `vm default-backend`.
- `DELONIX_SERVICE_VIP_CIDR` (lista separada por vírgulas) sobrepõe-se para um processo (CI,
  laboratório). O `show` diz que fonte está em vigor.
- Exemplo de laboratório: `delonix network vip-pool set 10.90.0.0/16`.

**O que um pool tem de ser:** IPv4, dentro do RFC 1918 ou de `100.64.0.0/10`, prefixo entre `/16` e
`/29` (uma gama pública sombrearia endereços reais da Internet para todos os containers).

**O que não pode sobrepor, e como se detecta cada um** (verificado no `vip-pool set`, em cada
alocação ou reserva de VIP, e em cada `stack plan`/`apply` com um `Service` `VirtualIP`):

| Não pode sobrepor | Detectado por |
|---|---|
| Redes declaradas do motor | os registos do `NetworkStore` (`base=`, `cidr=`) e o registo `NetDef` do holder em `<root>/ingress/` — ficheiros, sem holder |
| Redes de pods | as configs CNI que o CRI lê (o mesmo directório que o `cni::readiness` lê): `ipam.subnet` e cada `ipam.ranges[][].subnet`; e o `podSubnet` de cada cluster em modo kind, em `<root>/clusters/<nome>/kubeadm.conf` |
| Subredes de serviços | o `serviceSubnet` de cada cluster em modo kind, no mesmo ficheiro; os VIPs dos outros Services já ficam excluídos pelo IPAM do próprio pool |
| Endereços e rotas do host | `ip -4 -o addr show` e `ip -4 route show table all`, corridos pela CLI no netns do **host** antes de falar com o holder; todo o prefixo de endereço e de rota excepto a rota por omissão. Apanha a LAN, `docker0`/`virbr0` e qualquer VPN que instale rotas |
| Gamas de VPN / overlay | os overlays do motor no `NetworkStore` (redes `wg_ip` e IPs dos nós pares); interfaces WireGuard do host por `ip -4 -o addr show type wireguard` e as rotas por elas (o que o `wg-quick` instala para os `AllowedIPs`) |
| A canalização do próprio holder | a subrede do slirp `10.0.2.0/24` e os endereços das bridges do holder |

Limite conhecido da detecção, escrito: um par WireGuard cujos `AllowedIPs` **não** são encaminhados
(`Table = off`) é invisível — `wg show allowed-ips` precisa de `CAP_NET_ADMIN` no host, que o motor
não tem. Os clusters Kubernetes montados por SSH (`mode: ssh`) guardam as subredes no manifesto e
não neste nó; só importam aqui se forem encaminhados aqui, e isso a verificação de rotas vê.

**Quando a sobreposição aparece depois** (uma VPN levantada depois de o VIP existir): o `plan`/`apply`
seguinte recusa com conflito (exit 5), a nomear o VIP e a fonte sobreposta. O dataplane **não** é
retirado em silêncio — tirar um VIP debaixo dos clientes é decisão do operador, movendo o pool ou o
VIP.

**Alocação:** lease chaveado `svc:<namespace>/<nome>`, guardado em `ServiceDef.vip`, libertado no
`delete`/`--prune`/`destroy`. `spec.vip` explícito é **reservado** (`ipam::reserve`) ou recusado:
fora do pool → DX novo de classe *invalid*; já usado por outro serviço → `Conflict` (exit 5). O
`prune::lease_owners` conta os serviços declarados como donos — sem isso o ceifador do IPAM reclama
o VIP e entrega-o ao serviço seguinte.

**Rejeitado:** o VIP derivado por hash (`service_vip`) — 50 % de colisão aos 300 serviços, sem
registo onde a ver.

### D3 — Dataplane: uma cadeia `svc`, reescrita inteira e atómica; uma recusa explícita para o resto

- O ruleset base do holder ganha `chain svc` (nat, sem hook), um `jump svc` no `pre`, e uma cadeia
  `out` (`type nat hook output priority -100; jump svc`) para clientes do próprio holder (o proxy L7
  pode ter um `Service` como backend). O holder já tem rota por omissão pelo `tap0`, o que o E4
  mostrou ser necessário.
- Por VIP e porta **com pelo menos um backend pronto**:
  `ip daddr <vip> tcp dport <port> dnat ip to numgen random mod <N> map { … }` sobre os backends
  prontos apenas. **`numgen random`, não `inc`**: o `random` não tem estado, o `inc` recomeça a
  cada reescrita (E10). A distribuição medida (E1) é a de um gerador uniforme.
- **Nenhum backend pronto → indisponibilidade previsível.** O VIP fica sem regra de DNAT, e uma
  cadeia de filtro `vipguard` (`type filter hook forward priority -20`, e o mesmo em `output`)
  recusa todo o endereço dos pools configurados que nenhum DNAT reescreveu:
  `meta l4proto tcp reject with tcp reset`, o resto `reject with icmp type port-unreachable`.
  Medido (E14): o cliente recebe `ConnectionRefused` de imediato, TCP e UDP. **Nunca** um backend
  ainda a arrancar, nunca um timeout, e nunca a rota por omissão: sem este filtro os SYN saem do
  holder pelo `tap0` (E14, 3 pacotes contados) e o cliente espera pelo timeout.
- O mesmo filtro recusa uma porta não servida do VIP (E15), pela mesma razão de fuga.
- **A reescrita é sempre total**: `flush chain ip dlxing svc` + todas as regras num só `nft -f`
  (E10: zero erros com tráfego; E12: um script inválido não aplica nada). Sem edições incrementais
  de mapas, logo sem a classe de bug de ordem do `@netpair`. Custo: ~50 ms para 200 × 3 (E11).
- Só TCP no v1. Um VIP UDP precisa de um sinal de prontidão que a sonda TCP não dá (D8), e entra
  quando houver um uso concreto.

### D4 — Alcance e isolamento não mudam

O DNAT corre em `prerouting`; as cadeias por container (`fwout` −6, `fwcont` −5) correm em
`forward`, **depois**, sobre o backend real (E5). O isolamento de namespace, o `NetworkPolicy`, o
`Dependency` e o `NetworkAccessRule` aplicam-se como se o cliente tivesse usado o IP do backend. Um
VIP não fura fronteira nenhuma: um cliente de outra namespace chega ao VIP e é recusado pela cadeia
de cada backend, tal como seria no IP directo.

### D5 — Hairpin: exige `br_netfilter`, e recusa sem ele

Com o cliente na bridge dos backends — o caso comum — o hairpin só funciona com
`bridge-nf-call-iptables=1` (E3: 60/60 timeouts sem ele). A alternativa, um `masquerade` do tráfego
DNAT que volta à mesma bridge, funciona mas faz o backend ver o gateway em vez do cliente (E3), e
tira à aplicação o IP de origem. Decisão: **sem masquerade**; `type: VirtualIP` é recusado com
classe 69 quando o holder não tem `/proc/sys/net/bridge/bridge-nf-call-iptables` a `1` — a mesma
pré-condição e a mesma classe da D5 do plano. O holder põe-no a `1` no seu netns (gravável pelo
root do userns, E3); só recusa quando o módulo falta no host.

### D6 — Publicar o VIP no host: o caminho do `-p` que já existe

`spec.publish: [<hostPort>]` usa o `slirp_add_hostfwd` de sempre (bind a `127.0.0.1` por omissão,
`DELONIX_PUBLISH_ADDR` para alargar) e põe na cadeia `svc`
`ip daddr 10.0.2.100 tcp dport <hostPort> dnat ip to numgen random mod N map { … }` — o mesmo
conjunto, outra entrada (E9). **Sem duplo DNAT** (para o VIP e depois para o backend): um DNAT em
`prerouting` é terminal, por isso a regra aponta directamente aos backends. Sem backend pronto, a
porta publicada recebe a mesma recusa. A origem chega intacta para clientes encaminháveis e como
`10.0.2.2` para os de loopback (a regra já no AGENTS.md).

### D7 — O conjunto mantém-se actual sem daemon

- **Uma função, dois consumidores:** `service_backends(def, containers)` passa a ser a única
  avaliação do selector (mesma namespace, `matches_labels`, workload vivo por pid + `starttime`,
  com IP numa rede). O índice DNS e a reescrita do dataplane usam-na; a prontidão (D8) estreita o
  resultado para o dataplane. Não há segunda leitura do `matchLabels`.
- **Quem reescreve:** o processo de controlo do holder, num verbo novo `svcsync` — o mesmo
  processo que já constrói o índice DNS a partir dos mesmos registos. Só existe enquanto a infra
  existe, que é exactamente quando há VIPs para servir.
- **Quando:** (1) `apply`/`delete` de um `Service` (CLI → `svcsync`); (2) no fim do `do_attach` e
  do `do_detach` (já dentro do holder — chamada directa); (3) quando o supervisor grava a morte de
  um container, porque uma morte sem `detach` deixaria o IP no mapa; (4) nas transições de
  prontidão (D8); (5) no arranque do plano de controlo — cobre o reinício do controlo e a
  reconstrução completa.
- **Sem reconciliação periódica da pertença.** Uma mudança que nenhum dos cinco caminhos vê (um
  registo editado à mão) espera pelo próximo evento. Escrito, não escondido.

### D8 — Prontidão: o que admite um backend em rotação

**Admissão = (a) E (b):**

- **(a) A porta declarada aceita uma ligação TCP feita de dentro do holder** — sempre, para todo o
  backend, com ou sem sonda. É o critério mínimo explícito: prova que algo escuta onde o VIP vai
  mandar o tráfego. **Não** se chama «saudável»: o `get services` mostra `ready (tcp)`, e um
  container nunca é dado como saudável por o processo ter arrancado.
- **(b) Quando o container tem `HealthConfig`** (`--health-cmd` ou um `HEALTHCHECK` monitorizado):
  `health_state.health == Healthy`. `Starting` fica fora; `Unhealthy` fica fora.

Um container sem sonda de saúde é admitido só por (a); um cuja porta não aceita fica fora, seja
qual for o estado do processo. Sem nada admitido, aplica-se a recusa da D3.

**Quem corre (a):** uma thread de sonda no processo de controlo do holder — o processo que já corre
os servidores de DNS, RA e DHCP como threads e vive exactamente tanto quanto a infra. Só arranca
quando existe pelo menos um `Service` `VirtualIP`. Nenhum processo novo, nenhum ciclo de vida
próprio. É, dito sem rodeios, uma actividade **periódica** nova dentro de um processo residente que
já existe; é o que a decisão 3 do dono exige, e é a forma menos residente de a cumprir.

- Omissões: a cada 2 s, timeout de 500 ms, 1 sucesso para admitir, 2 falhas para ejectar;
  sobreponíveis por `Service` em `spec.readiness.tcp`.
- As sondas correm em paralelo com tecto, para o timeout de 500 ms de um backend filtrado (E13) não
  atrasar os outros; uma ligação recusada ou aceite custa < 0,1 ms (E13).
- Estado em memória. **Depois de um reinício do controlo todos os backends começam NÃO prontos** e
  são admitidos no primeiro sucesso (fail-closed): o custo é até um intervalo de
  `ConnectionRefused` no VIP — medido no cenário de caos, não suposto.
- A sonda nasce no holder (hook `output`), por isso as cadeias de forward por container não a
  filtram: mede «a aplicação escuta», não «este cliente é permitido» — o que a D4 deixa à cadeia do
  próprio backend, sem mudança.

**Quem corre (b):** o monitor do supervisor que já existe. Numa transição, o `health_monitor_loop`
envia `svcsync` logo a seguir a emitir o evento `health_status` (best-effort: holder em baixo =
nada a reescrever).

**As ligações já abertas a um backend ejectado continuam** (E8): o conntrack guarda o DNAT e um
pedido a meio acaba. O conntrack não é limpo na ejecção. Na morte do container não há nada a limpar.

| Opção avaliada | Custo | Veredicto |
|---|---|---|
| Só o monitor de saúde do supervisor | Zero actividade nova; mas um container sem sonda seria admitido «porque arrancou», o que a decisão 3 proíbe | Fica como (b), insuficiente sozinho |
| **Thread de sonda TCP no processo de controlo do holder** | Tarefa periódica num processo residente existente; estado perdido num reinício do controlo (fail-closed); testa «escuta», não «pronto» | **Fica como (a), no plano principal** |
| Timer de systemd por Service | Depende de sessão systemd de utilizador; um processo por tick; granularidade grosseira por omissão (`AccuracySec=1min`); caminhos root/rootless diferentes | Rejeitada |
| Sondar só nos eventos (`stack`/`container`) | Nenhum processo; um backend pendurado nunca é ejectado | Rejeitada como mecanismo único (é a D7) |
| Um processo supervisor por Service | Um daemon por serviço, com ciclo de vida, guarda de identidade de pid e reaping próprios | Rejeitada: o que o guarda-rio 1 proíbe sem necessidade provada |
| Nada | E6: 1/N das ligações recusadas enquanto um backend morto está no mapa | Rejeitada |

### D9 — O par `lbset`/`lbclear` e o `service_vip` saem

`do_lbset`, `do_lbclear`, os verbos `lbset`/`lbclear` do socket, `set_service_lb`,
`set_service_lb_algo`, `clear_service_lb`, `delonix_net_rules::service_vip` e
`Error::InvalidLbSpec` são retirados. Não têm chamadores, o par não aceita o VIP que o hash produz,
e a reescrita não é atómica. É quebra para quem usa a **biblioteca** `delonix-sdn` (a mesma nota que
a remoção do `Net` deixou); vai nas notas de release. Se saem de imediato ou depois de uma release
com `#[deprecated]` é a questão aberta 2.

## Alternativas consideradas

- **Só DNS (ADR-0032).** Não reequilibra uma ligação longa nem serve um cliente que guarda o IP, e a
  D3 aprovada pede L4.
- **IPVS.** Exige o módulo `ip_vs` e, para a maior parte da configuração, `CAP_NET_ADMIN` no
  namespace de utilizador inicial; **por leitura, não medido**. Não acrescenta nada que o `numgen`
  não dê para os conjuntos pequenos de um nó, e mete um segundo dataplane ao lado das nftables.
- **Proxy em espaço de utilizador** (alargar o proxy L7 a TCP). Copia bytes, processo residente
  por porta, e o IP de origem chega como o do proxy. O L7 continua a ser a resposta para HTTP; o L4
  é do kernel.
- **eBPF/XDP.** `CAP_BPF` não existe num userns sem privilégio; o motor já degrada o `net flow` pela
  mesma razão.
- **Afinidade `jhash ip saddr` por omissão.** Proposta como campo opcional
  `sessionAffinity: ClientIP` (fase F2b), não como omissão: remapeia sempre que N muda, e não foi
  medida neste spike (para não carregar o `nft_hash` no host).
- **Masquerade do hairpin** em vez de exigir o `br_netfilter` — rejeitado na D5 (perde o IP de
  origem, E3).
- **Um pool por omissão** (`10.90.0.0/16` reservado em todo o lado) — rejeitado pelo dono (decisão 1).
- **Sem recusa; deixar um VIP vazio seguir** — rejeitado: o E14 mediu a fuga pela rota por omissão
  e o timeout do cliente.

## Plano por fases

| Fase | O quê | Ficheiros |
|---|---|---|
| F1 — modelo e limpeza | `ServiceDef.{type, vip, publish, readiness}` (`#[serde(default)]`); comando `network vip-pool` e `<root>/ingress/vip-pool`; a detecção de sobreposição da D2 como função pura sobre as fontes recolhidas, mais um recolhedor fino; alocação de VIP no IPAM; `lease_owners` conta os serviços; schema e `explain`; `hot_fields` do `Service`; retirada da D9; DX novos e `pt.po` | `crates/adapters/delonix-sdn/src/{infra.rs,ipam.rs,cni.rs,error.rs}`, `crates/foundation/delonix-net-rules/src/lib.rs`, `crates/foundation/delonix-model/src/codes.rs`, `bins/delonix-runtime-bin/src/cmd/{service.rs,network.rs,prune.rs,schema.rs}`, `crates/contexts/delonix-stack/src/reconcile.rs`, `docs/schema/v1/delonix.json`, `data/pt.po` |
| F2 — dataplane | cadeias `svc`/`out` e `vipguard` no ruleset base (criadas também por um `svcsync` contra um holder antigo, sem duplicar o `jump`); `service_backends` partilhada com o índice DNS; verbo `svcsync` e os cinco gatilhos da D7; DNS de um `VirtualIP`; `publish` (D6); recusa sem `br_netfilter` (D5) | `crates/adapters/delonix-sdn/src/infra.rs`, `bins/delonix-runtime-bin/src/cmd/{service.rs,container.rs}`, `crates/adapters/delonix-linux/src/supervise.rs` |
| F3 — prontidão | a thread de sonda TCP no processo de controlo (D8 a); a porta de saúde (D8 b) e o `svcsync` no `health_monitor_loop`; `get/describe services` com `READY/TOTAL`, `ready (tcp)` vs `healthy` por backend, e o estado de recusa | `crates/adapters/delonix-sdn/src/infra.rs`, `bins/delonix-runtime-bin/src/cmd/{container.rs,service.rs}` |

A matriz passa `net.lb.l4` a `partial` no fim da F2 e a `supported` quando os checks abaixo existirem
e passarem; `net.lb.health-check` idem no fim da F3. A evidência citada é o título do check, como
manda o ADR-0050. **Nenhuma fase publica um VIP sem a porta de prontidão da F3 ligada**: a F2 sozinha
fica atrás de uma flag escondida para a bateria, para um VIP publicado nunca encaminhar para um
container ainda a arrancar.

### Gates

Testes puros (sem holder): a renderização do script `nft` (nenhum backend pronto → sem DNAT e o pool
no `vipguard`; N backends → mapa de N; `publish` → segunda regra; nomes e IPs validados antes do
argv); a validação do pool contra cada fonte da D2 (um fixture por fonte, incluindo uma listagem de
rotas do host e uma interface WireGuard); `service_backends` com prontidão (`Starting` fora, sem
sonda → só TCP, namespace); as recusas de VIP (fora do pool, duplicado); o `lease_owners` com um
lease `svc:`.

Bateria (`scripts/e2e.sh`, secção nova «kind: Service type VirtualIP», raiz isolada com os dois
roots):

- «sem pool de VIPs, type VirtualIP é recusado com classe 69»;
- «um pool de VIPs sobreposto a uma rede declarada, a uma gama CNI de pods ou a uma rota do host é
  recusado, a nomear a fonte»;
- «um VIP reparte as ligações por todos os backends prontos» — 300 ligações de um container de outra
  rede, cada backend com pelo menos 20 %;
- «um cliente na rede dos backends chega ao VIP e o backend vê o IP dele» (hairpin, D5);
- «o nome do serviço resolve para o VIP»;
- «um backend removido (`container rm`) deixa de receber ligações novas» — zero em 200, sem
  `stack apply`;
- «um backend cuja porta não aceita fica fora de rotação» (sem sonda definida: o mínimo TCP);
- «um backend a arrancar não recebe tráfego até a sonda de saúde passar» (`--health-cmd` que lê um
  ficheiro; `Starting` → fora, ficheiro presente → dentro, ficheiro apagado → fora);
- «um VIP sem backend pronto recusa de imediato» (`ConnectionRefused` em < 1 s, nada na rota por
  omissão do holder);
- «uma porta não servida do VIP é recusada»;
- «um cliente de outra namespace não passa do VIP» (D4);
- «a porta publicada leva ao mesmo conjunto»;
- «sem `br_netfilter` o type VirtualIP é recusado com classe 69» (só onde o módulo falta; senão
  `SKIP` audível);
- «o pool recusa um VIP duplicado (exit 5) e fora dele».

Caos (`scripts/chaos.sh`, cenário novo `service_lb`): três backends e um cliente em ciclo contínuo;
(1) `kill -9` a um backend com `--restart no` — o cliente vê erros no máximo até o supervisor gravar
a morte, e depois zero; (2) uma sonda de saúde a falhar num segundo backend — ejectado em
≤ `interval × retries` + 2 s; (3) um backend que fecha a porta com o processo de pé — ejectado em
≤ `failureThreshold × interval` da sonda TCP + 1 s; (4) `kill -9` ao plano de controlo do holder — o
pin guarda o ruleset, o tráfego **não pára** para os backends já no mapa até o controlo reiniciado
reescrever; o cenário mede as recusas na janela em que todos os backends recomeçam não prontos;
(5) `netns down`/`up` (morte do pin) — o VIP volta com o mesmo endereço (vem do registo). O cenário
compara as contagens por backend e não só «o cliente recebeu respostas», porque um VIP que servisse
um só backend também responderia.

## Consequências

- O `Service` passa a ter duas formas de ser servido sob o mesmo Kind; a omissão não muda.
- O holder ganha uma responsabilidade (reescrever a cadeia `svc`) e uma tarefa periódica (a sonda
  TCP de prontidão), sem processo novo.
- Um nó sem pool configurado não tem VIPs, de propósito; o erro diz como configurar um.
- A verificação de sobreposição do pool depende do que o motor consegue ver sem privilégio no host;
  a lacuna do WireGuard não encaminhado fica documentada.
- A velocidade de ejecção é a da sonda: 2 × 2 s para a verificação TCP por omissão,
  `interval × retries` para um comando de saúde (omissões do Docker: 90 s; a documentação do
  `Service` recomenda valores curtos).
- Sai uma API pública da biblioteca `delonix-sdn` (D9).
- IPv6 fica de fora (a D4 do plano trata a `table inet`); o VIP é IPv4 como o resto do dataplane.

## Questões em aberto para o dono

1. **`port` único no v1, ou já `ports: [{port, targetPort, protocol}]`?** Recomendação: o `port`
   único no v1 (uma regra, um alvo de sonda, a bateria acima), com `ports[]` numa fase própria — a
   recusa, o estado de prontidão e o publish passam a ser por porta, e isso merece medições próprias.
2. **D9 — retirar `set_service_lb*`/`service_vip` de imediato, ou uma release com `#[deprecated]`?**
   Recomendação: retirar de imediato. Não têm chamador no workspace e não funcionam (o par recusa o
   VIP do hash), por isso um período de depreciação só manteria visível uma API partida; a quebra é
   nomeada nas notas de release.
