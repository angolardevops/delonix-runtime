# ADR-0065: IPv6 no dataplane da SDN — uma tabela `inet`, endereços derivados do lease v4, anti-spoof na família `bridge`

- **Estado:** Proposto (2026-10-02). Nada implementado; o spike está feito e registado abaixo.
- **Data:** 2026-10-02
- **Decisores:** Walter Angolar
- **Relaciona-se com:** decisão D4 do plano de maturidade (`docs/discovery/65_PLANO_MATURIDADE.md`,
  PR #658: «IPv6 no dataplane — implementar»); a recusa de IPv6 da v0.37.1 (Bloco 0 do plano 33,
  `AGENTS.md`); a célula `net.ipv6` da matriz de capacidades (ADR-0050); os guarda-rios 1, 5 e 6
  da `delonix-adr` (daemonless, spike antes de uma fronteira de privilégio, sem falha silenciosa).

## Contexto

### O que existe hoje (lido no código, `origin/main` b269c46e)

- **A firewall inteira é `table ip dlxing`** (`ingress_table_ruleset`, `crates/adapters/delonix-sdn/src/infra.rs`):
  `@dlxall`, os `@dlxns<hash>`, o verdict map `@fwmap` (`ipv4_addr : verdict`), as chains
  `fwguard` (-20), `fwdeny` (-10), `fwout` (-6), `fwcont` (-5), `dlxinput` e `forward` (0, `policy
  drop`). O corpo de cada chain de workload sai de `policy_nft::chain_body`, todo ancorado em
  `ip daddr`/`ip saddr`. São 59 referências a `INGRESS_TABLE` só no `infra.rs`, quase todas com o
  argumento de família `"ip"`.
- **O IPv6 é recusado em duas camadas** desde a v0.37.1: `disable_ipv6` no netns de cada container
  (`disable_ipv6_argv`, no `do_attach` e no `attach-extra`) e `table ip6 dlxing` com `forward
  policy drop` no holder (`ingress_v6_refusal_ruleset`). `DELONIX_ENABLE_IPV6=1` repõe o caminho
  antigo, sem política nenhuma. O motivo está medido e escrito: a ULA `fd00:<o2>::<o3>:<o4>`
  contornava toda a política, porque toda ela é `table ip`.
- **O esquema de endereços antigo não serve para reaproveitar**: `fd00:<grupo>::/64` com o
  segundo octeto v4 escrito em DECIMAL dentro de um campo hexadecimal, sem Global ID aleatório
  (RFC 4193 §3.2), e partido para as redes CIDR (`172.20.4.0/22` não tem «segundo octeto» que
  identifique a rede). E colide em espírito com o `fd00::/64` fixo do slirp (ver S5).
- **Restos de v6 que continuam ligados com a recusa activa** (lido, não medido ao vivo): o
  `ensure_net_bridge` põe `fd00:<grupo>::1/64` em cada bridge e liga `ipv6/conf/all/forwarding`, e o
  `ra_sender_main` (thread do plano de controlo) emite RAs desse prefixo em todas as bridges, sem
  condição. Os containers não os vêem (v6 desligado); uma VM Cloud Hypervisor na SDN, que não
  passa pelo `disable_ipv6`, ganha um endereço SLAAC. O `dlxinput` é `table ip`: o tráfego v6 de
  uma VM PARA o holder não tem política nenhuma. Se algum serviço do holder escuta em v6 não foi
  medido.
- **DNS**: `dns_action_owned` responde NODATA a um `AAAA` de um nome nosso — correcto hoje, porque
  «não há AAAA» é a verdade.
- **Linha de controlo**: `attach <netns> <ip> <bridge> <gw> [<ns>]`, `attach-extra …`,
  `vmtap …`; `validate_control_tokens` exige IPv4 estrito (`control_ipv4_ok`) em cada IP.
- **Saída**: o slirp do holder corre sem `--enable-ipv6`.

### O spike (2026-10-02, kernel 7.0.0-34-generic, nftables 1.0.9, slirp4netns 1.2.1/libslirp 4.7.0)

Tudo em namespaces descartáveis (`unshare --user --map-root-user --net [--mount]`), sem root, sem
tocar em sysctls nem módulos do host e sem o motor. Os scripts e a saída de cada corrida estão em
`docs/adr/0065-ipv6-spike/` (correr a partir dessa pasta, por exemplo
`unshare --user --map-root-user --net --mount sh ./s2.sh`; o `s4.sh`/`s5.sh` correm sem `unshare`,
criam eles o namespace, e precisam de um caminho curto para o socket do slirp, por isso usam
`mktemp -d /tmp/dlx65.XXXX` e apagam-no no fim). O host tem o `br_netfilter` carregado.

**S1 — o `nft` aceita uma `table inet` com as estruturas de hoje, em userns** (`s1.nft`, rc=0).
Na mesma tabela: sets `ipv4_addr` e `ipv6_addr`, um verdict map por família (`fwmap4`,
`fwmap6`) a saltar para a MESMA chain de workload, o `netpair` (`ifname . ifname : verdict` com
`counter`), `ct state`, `icmpv6 type …`, `ip6 daddr fe80::/10`, e uma chain `nat` com
`masquerade` aplicável às duas famílias (`ip6 saddr fd65:1::/64 oifname "tap0" masquerade`). Um
mapa de uma família só porque o tipo da chave é um só: `ipv4_addr` e `ipv6_addr` não cabem no
mesmo mapa. A família `bridge` também carrega em userns, com `icmpv6 taddr` (o alvo de um NA) —
a forma `@th,64,128` não é aceite pelo parser desta versão.

**S2 — o caminho `bridge-nf` para v6 é por netns e liga-se por omissão** (`s2.sh`). Num netns
novo, `/proc/sys/net/bridge/bridge-nf-call-ip6tables` e `-iptables` valem **1**, e escrevem-se de
dentro do userns (0 e 1, os dois «ok»). Com 0, a `table inet` não vê tráfego entre duas portas da
mesma bridge (todos os contadores a 0; um ping a→b passa). Com 1 vê (`a2b-seen: 2`, `ra-seen: 1`,
`nd-seen: 2`). O tráfego encaminhado entre duas bridges é visto nos dois casos (`inter-bridge-drop`
conta em ambos). **E com `bridge-nf` a 1 o `iifname` de um pacote em ponte é a BRIDGE (`br0`), não
a porta**: `spoof6-seen-iif-bridge: 2`, `spoof6-seen-iif-veth: 0`.

**S3 — consequência medida no IPv4 de hoje: o anti-spoof por veth é inerte** (`s3.sh`). A forma de
produção (`insert rule ip dlxing fwdeny iifname <veth> ip saddr != <ip> drop`,
`antispoof_rule_args`) ficou com **0 pacotes**, e um container a falsificar a origem chegou a um
vizinho da mesma bridge (3/3) e a um container de outra bridge por rota (3/3). Os dois factos de
S2 explicam-no: para tráfego em ponte o `iifname` é a bridge, e para tráfego encaminhado o pacote
entra na pilha IP pela bridge também. O mesmo vale para o `tap` de uma VM (é porta de uma bridge),
ou seja a correcção do anti-spoof do `tap` da auditoria #3 está escrita mas não filtra. Este achado
é sobre o IPv4 que está em produção e não espera por este ADR (ver «Questões para o dono»).

**S6 — o isolamento por namespace funciona em v6 com a mesma forma** (`s6.sh`, `bridge-nf` a 1).
Uma chain de workload com as regras v4 e v6 lado a lado, alcançada por `fwmap4` e `fwmap6`:

```
v4 a(X)->b(Y): BLOCKED      v6 a(X)->b(Y): BLOCKED
v4 c(Y)->b(Y): OPEN         v6 c(Y)->b(Y): OPEN
v4 b(Y)->a(X) (retorno por established): OPEN     v6 idem: OPEN
ip6 daddr fd65:1::b ip6 saddr @dlxall6 ct state new counter packets 2
```

O ND entre vizinhos passou sem regra própria (o NS vai para o multicast solicited-node, que não
casa com `ip6 daddr <workload>`).

**S8 — anti-spoof na família `bridge` funciona sem `br_netfilter`** (`s7.nft` + `s8.sh`, com os
dois `bridge-nf-call-*` a **0**). Uma chain `bridge … prerouting` com sets `ifname . ether_addr`,
`ifname . ipv4_addr` e `ifname . ipv6_addr` (endereço atribuído + o link-local da porta):

```
legit v4 a->b: replies   legit v6 a->b: replies   legit v6 a->holder gw: replies
spoof v4 a->b: no reply  spoof v4 a->holder: no reply
spoof v6 a->b: no reply  spoof v6 a->holder: no reply
b got rogue RA prefix: 0
```

No `prerouting` da família `bridge` o filtro apanha também o que vai PARA o holder (o `forward`
não o veria). Em S2, sem esta chain, um RA forjado pelo container `a` deu um prefixo `fd77::/64`
ao vizinho `b`; com ela, `ra-drop` contou 1 e `b` não ganhou nada. O NA falsificado (`icmpv6
taddr` fora dos endereços da porta) tem regra carregada e **não foi exercitado** com tráfego.

**S4/S5 — o que o `slirp4netns --enable-ipv6` dá** (`s4.sh`, `s5.sh`):

- Um RA no `tap0` com o prefixo **fixo** `fd00::/64` e default `via fe80::2`; o gateway
  `fd00::2` responde a ping. O prefixo não é configurável nesta versão (`--cidr` é só v4; não há
  opção v6 de prefixo no `--help`).
- Com `ipv6/conf/all/forwarding=1` (o holder encaminha), o `tap0` só aceita o RA com
  `accept_ra=2` — medido com 2; o caso 1 não foi medido, é o comportamento documentado do kernel.
- **Uma origem fora de `fd00::/64` não passa pelo slirp** (`container fd65:1::a -> fd00::2
  WITHOUT nat66: FAIL`); com `oifname "tap0" ip6 saddr … masquerade` numa `table inet` passa
  (`masquerade packets 1`). O egress v6 exige NAT66 no holder, como o v4 já faz masquerade.
- O DNS v6 do slirp (`fd00::3`) **não respondeu** (timeout); o `10.0.2.3` respondeu a uma pergunta
  `AAAA` com duas respostas. O resolvedor do holder continua a usar o caminho v4.
- **O `add_hostfwd` da API do slirp4netns 1.2.1 é só IPv4**: `guest_addr` v6 e `host_addr` `::1`
  são recusados (`bad arguments.guest_addr`, `bad arguments.host_addr`). Publicar uma porta em v6
  no host não se faz por este slirp.
- **Saída v6 para a Internet: não medível nesta máquina** — o host não tem endereço v6 global nem
  rota default v6 (`ip -6 route` só tem `fe80::/64`), e o slirp abre os sockets do lado do host:
  `tcp6 … 443` deu `Network is unreachable`. O mesmo pedido em v4 ligou.

## Decisão

### D1 — Uma só `table inet dlxing`, não uma `table ip6` paralela

A política passa a viver numa `table inet dlxing` que substitui as duas de hoje (`ip dlxing` e a
recusa `ip6 dlxing`). Dentro dela: `@dlxall4`/`@dlxall6`, os sets de namespace aos pares
(`dlxns4<hash>`/`dlxns6<hash>`), `@fwmap4`/`@fwmap6` a saltar para a mesma chain de workload, e
as chains que não comparam endereços (`fwdeny` com o `netpair`, `forward`, `fwguard`) ficam uma
só, porque `ifname`, `ct state` e o verdict `drop` valem para as duas famílias.

Razão: o motor já pagou duas vezes por duas cópias do mesmo formato (`fw_rule_tail`, as seis listas
de Kinds). Com uma `table ip6` paralela cada regra teria dois geradores, e o contorno de v0.37.1
voltava no dia em que um deles ficasse para trás. Numa só tabela o `policy_nft::chain_body` emite,
para cada regra, a linha `ip` e a linha `ip6` no mesmo sítio, e o teste que já fixa o corpo passa
a fixar as duas.

Os nomes `fwmap`/`dlxall` mudam (passam a ter sufixo de família), e com eles os parsers que lêem
`nft list map ip dlxing fwmap` (`parse_fwmap_elements`, o `ingress ls`).

**Migração**: nenhuma a quente. A família da tabela é decidida quando a netns de infra é
CONSTRUÍDA. Um plano de controlo novo que reata a um pin antigo (tabela `ip dlxing` presente)
mantém o modo v4 tal como está e **recusa** uma rede com IPv6 a nomear o remédio (`delonix net
netns down` + `up`, que reinicia os workloads — é decisão do operador, a mesma regra do
`stale_holder_message`). Converter a tabela por baixo de containers vivos não entra: o estado do
`@fwmap` e dos sets teria de ser reconstruído dos registos numa só transacção, e um erro aí deixa
o nó sem política.

### D2 — Anti-spoof na família `bridge`, nas três famílias de endereço, e o v4 de hoje entra primeiro

O anti-spoof sai do `fwdeny` (onde S3 mediu que não filtra) para uma `table bridge dlxspoof` com uma
chain `prerouting` (prioridade -300) e quatro sets por porta: `ifname . ether_addr`, `ifname .
ipv4_addr`, `ifname . ipv6_addr` (o endereço atribuído e o link-local da porta) e `ports`. As regras
são as de `s7.nft`: MAC de origem, `arp saddr ip`, `ip saddr`, `ip6 saddr` (exceptuando `::`),
`icmpv6 type { nd-router-advert, nd-redirect }` e NA com `taddr` fora dos endereços da porta,
e `udp sport 547` (um container não é servidor DHCPv6).

Vale para o veth do container, o veth extra e o `tap` de uma VM — uma só função de elementos, como
o `antispoof_rule_args` era para uma só regra. Não depende do `br_netfilter` (S8), por isso também
cobre o host onde o `bridge-nf` estiver desligado.

A parte v4 corrige um defeito que está em produção e é a fase P0 deste plano; se o dono preferir,
sai antes deste ADR ser aceite, como correcção de segurança à parte.

### D3 — `bridge-nf-call-ip6tables` posto pelo holder, e verificado

O isolamento dentro da mesma rede (namespace, `NetworkPolicy`, `Dependency`) decide em `forward`,
e para tráfego em ponte isso só existe com `bridge-nf-call-ip6tables=1` no netns do holder (S2).
O holder escreve `bridge-nf-call-iptables=1` e `bridge-nf-call-ip6tables=1` no seu próprio netns
ao construir a infra (é por netns e escreve-se em userns — medido), e LÊ-OS de volta. Um valor
que não ficou a 1, ou a ausência de `/proc/sys/net/bridge` (módulo `br_netfilter` não carregado
no host), recusa a criação de uma rede com IPv6 com classe 69 (pré-condição do host). É a regra da
D5 do plano de maturidade aplicada ao v6, e é mais estrita: sem `bridge-nf` o v6 não liga de todo,
não liga «com aviso».

### D4 — Endereços v6 derivados do lease v4: um só registo de leases

- **Prefixo do nó**: um Global ID aleatório de 40 bits (RFC 4193), gerado uma vez e guardado em
  `<state root>/ipam/ula-global-id`. Fica `fdXX:XXXX:XXXX::/48` para o nó.
- **Prefixo da rede**: um `/64` dentro do `/48`, com um subnet-id de 16 bits escolhido no
  `network create` (o primeiro livre) e PERSISTIDO no `NetDef` (`ipv6_prefix: Option<String>`,
  `#[serde(default)]`; `None` é «rede sem v6», que é o que todo o registo existente significa).
  Um `--ipv6-subnet <prefixo>/64` declarado é aceite se for ULA ou GUA `/64`, não sobrepuser outra
  rede do nó e não for `fd00::/64` (o do slirp, S4).
- **Endereço do workload**: o prefixo da rede mais os 32 bits do endereço v4 alugado no `/64`
  (`10.200.0.7` = `0x0ac80007` → `<prefixo>::ac8:7`). O gateway é o mesmo derivado do gateway
  v4.

Porque não um segundo IPAM: o motor já tem um registo de leases cujo ceifador só chegou depois de
88 % das entradas estarem órfãs (`network ipam prune`, 2026-09-15). Um segundo registo era um
segundo ceifador e uma segunda forma de os dois discordarem. Derivado do v4, o v6 nasce, muda e
morre com o lease que já existe, um `--ip` fixo dá também o v6 fixo, e a anti-spoof e a política
conhecem o endereço antes de o workload arrancar.

Consequência assumida: só **dual-stack**. Uma rede só-v6 fica fora deste ADR.

### D5 — Atribuição estática nos containers; sem RA nas redes com política

No `attach`, o v6 é posto como o v4: `ip -6 addr add … nodad`, `ip -6 route add default via
<gw6>`, e `accept_ra=0` no netns do container (defesa em profundidade com a D2). O `ra_sender_main`
deixa de emitir RAs nas bridges com política; e a bridge só recebe endereço v6 quando a rede tem
`ipv6_prefix` (hoje recebe sempre, ver Contexto). As VMs entram depois (P5) com o v6 escrito no
`network-config` do seed, que já casa a NIC pelo MAC: um endereço SLAAC com privacy/stable-privacy
no convidado não é previsível do lado do host, e a anti-spoof recusá-lo-ia. Um RA só com a rota
default (A=0) fica como alternativa se o `network-config` não chegar.

### D6 — DNS `AAAA` no holder

O índice do DNS passa a guardar, por nome, o v4 e o prefixo v6 da rede, e o `AAAA` responde com o
endereço derivado (D4). Os `Service` (ADR-0032) devolvem um `AAAA` por backend, com a mesma rotação
e o mesmo isolamento de namespace do `A`. Um nome nosso numa rede sem v6 continua NODATA. O
reencaminhamento de perguntas externas continua pelo caminho v4 (S4: o `10.0.2.3` respondeu a um
`AAAA`, o `fd00::3` não).

### D7 — Saída: slirp com `--enable-ipv6`, NAT66 no holder; publicação de portas só v4

O slirp do holder passa a `--enable-ipv6`, o `tap0` a `accept_ra=2` e a `table inet` ganha
`oifname "tap0" ip6 saddr <prefixos do nó> masquerade` (S5: sem NAT66 não passa nada). A saída v6
de um container só existe num host com v6 de saída; num host sem ele, o `connect` dá `Network is
unreachable` — o mesmo que hoje. A `egress` por rede e por container (`fwdeny`, `fwout`) aplica-se
às duas famílias pelas linhas `ip6` do D1. O `fwguard` ganha `fe80::/10`, `::1`, `fd00::/64`
(a rede do slirp, onde está o DNS e o gateway dele) e o endereço de metadata v6 que um provider
publique, à mesma prioridade -20 que o v4.

`-p`/`publish` continua só IPv4 (S4: o `add_hostfwd` do slirp4netns 1.2.1 recusa v6). Um
`-p [::1]:8080:80` ou um `--publish-addr` v6 é recusado com a razão, como o head não-IPv4 já é.

### D8 — Opt-in por rede; a recusa actual continua a omissão até o portão provar paridade

`delonix network create --ipv6` (e `spec.ipv6: true` no `kind: Network`) liga o v6 nessa rede.
Sem a flag, tudo fica como hoje: `disable_ipv6` no netns, e a tabela `inet` com uma regra
`meta nfproto ipv6 drop` nas bridges sem `ipv6_prefix` (substitui a `table ip6 dlxing` como
segunda camada). `DELONIX_ENABLE_IPV6=1` é removido no fim do plano: era a porta para o caminho
sem política, e com o v6 a sério deixa de ter razão de existir; até lá recusa ligar-se ao lado de
uma rede `--ipv6` (as duas juntas eram v6 com e sem política no mesmo nó).

O plano de maturidade diz «a recusa actual passa a opt-out». Este ADR propõe chegar lá em duas
fases: opt-in enquanto o portão (D10) não estiver verde numa release, e só depois a omissão muda,
numa major, com nota de quebra.

### D9 — Linha de controlo: verbo novo, nunca um token a mais nos verbos antigos

O v6 de um workload segue num verbo próprio, `addr6 <netns> <ifname> <ip6> <gw6>`, enviado
depois do `attach`/`attach-extra` (e `vmtap6` para as VMs na P5). Um holder antigo responde
`invalid control command`: o cliente desfaz o attach v4 que acabou de fazer e falha com a razão
(«o plano de controlo é anterior ao IPv6: `delonix net netns down` + `up`»). Nunca se fica com
um workload só-v4 numa rede declarada `--ipv6`. O `validate_control_tokens` ganha
`control_ipv6_ok` (parse estrito, sem zona `%`, sem mapeado-em-v4). Os verbos e a forma das
linhas existentes não mudam: um cliente antigo contra um holder novo continua a funcionar
(a mesma regra do `attach` de 5/6 tokens).

### D10 — O portão: cada propriedade de isolamento v4 provada também em v6

A célula `net.ipv6` só passa de `unsupported-by-provider` a `supported` (e o opt-in só pode virar
omissão) quando estes checks existirem e estiverem verdes. Cada um envia pacotes e mede no destino
ou num contador; ler o ruleset não conta (S3 é exactamente esse erro).

Na bateria (`scripts/e2e.sh`, secção nova «network: IPv6», só com `--ipv6`):

1. mesma rede, mesmo namespace: aberto em v4 e v6;
2. mesma rede, namespaces diferentes: bloqueado em v4 e v6, nos dois sentidos de iniciação; o
   retorno de uma ligação estabelecida passa;
3. `kind: Dependency` a→b: fura em v6 como em v4, e b→a continua fechado;
4. `ingress policy deny` + `ingress allow <porta> --from <cidr v6>`: só a origem permitida passa;
   `egress policy deny`: a saída v6 fecha;
5. duas redes sem `NetworkRoute`: bloqueado em v6; com ela, aberto;
6. multi-homing (`network connect`): o endereço v6 extra está debaixo da mesma firewall e no mesmo
   set de namespace (a regressão da varredura #2, agora em v6);
7. anti-spoof: origem v6 forjada não chega a um vizinho nem ao holder; RA forjado não dá prefixo
   a um vizinho; NA com alvo alheio não muda o vizinho do gateway;
8. `fwguard`: `fe80::`, `::1` e `fd00::/64` não alcançáveis por forward;
9. DNS: `AAAA` do próprio nome, de um nome noutro namespace (NXDOMAIN), de um `Service`
   (um registo por backend);
10. rede sem `--ipv6`: o container não tem endereço v6 nenhum (nem link-local), e um container
    `--privileged` que religue o v6 não encaminha nada;
11. holder antigo: um `network create --ipv6` contra um plano de controlo sem o verbo falha alto e
    não deixa o workload só-v4;
12. pré-condição: com `bridge-nf-call-ip6tables` a 0 (forçado no netns do holder) a rede `--ipv6`
    é recusada com classe 69.

No arnês de caos (`scripts/chaos.sh`), um cenário `ipv6_isolation_respawn`: dois namespaces numa
rede `--ipv6`, matar o plano de controlo e depois o pin, e repetir 1, 2 e 7 depois de cada
recuperação, com os PIDs dos workloads comparados (como o `control_restart`). Cada check e o
cenário têm de falhar com a peça respectiva revertida, verificado.

## Alternativas consideradas

- **`table ip6` paralela à `table ip`.** Mais barata de introduzir (não toca no v4). Rejeitada: dois
  geradores e dois leitores para cada regra, e a falha de um deles é a falha de v0.37.1 outra vez,
  em silêncio. Ver D1.
- **Converter `ip dlxing` → `inet dlxing` a quente no reattach.** Rejeitada: o estado do `@fwmap` e
  dos sets teria de ser reconstruído dos registos numa só transacção; um erro deixa o nó sem
  política, e o caso só existe num upgrade in-place, que tem remédio explícito.
- **Anti-spoof na `table inet` com `meta iifname`/`physdev`.** O `inet` vê a bridge como interface
  de entrada (S2) e o nftables não tem `physdev` nas famílias IP. Rejeitada por medição.
- **Segundo IPAM só v6 (lease próprio).** Rejeitada: segundo ceifador, segunda fonte de verdade
  (D4). O custo é não haver redes só-v6.
- **Manter o esquema `fd00:<o2>::<o3>:<o4>`.** Sem Global ID aleatório, decimal num campo
  hexadecimal, partido para redes CIDR, e vizinho do `fd00::/64` do slirp. Rejeitada.
- **SLAAC por RA para containers.** O endereço deixa de ser conhecido antes do arranque e passa a
  depender de extensões de privacidade do convidado; a anti-spoof teria de aprender endereços.
  Rejeitada para containers; para VMs fica como alternativa à D5.
- **DHCPv6 com estado no holder.** Mais um servidor por bridge para dar um endereço que o motor já
  sabe. Rejeitada nesta fase.
- **Ligar por omissão já.** É o que o plano de maturidade acaba por pedir. Rejeitada para já: o
  ponto de partida é um contorno de política medido, e a omissão só muda depois de o portão D10
  estar verde numa release (D8).
- **Não fazer nada (manter a recusa).** Continua a ser o comportamento de quem não passa `--ipv6`.
  Como fim, foi recusado pelo dono (D4 do plano de maturidade).

## Consequências

- Fica possível: v6 dual-stack por rede, com a mesma política, o mesmo isolamento e o mesmo DNS do
  v4; e a anti-spoof v4 passa a filtrar de facto.
- Fica mais difícil: toda a chamada `nft` do `infra.rs` muda de família, e os nomes de mapas e sets
  mudam — quem lê o ruleset do holder à mão (runbooks, a doc do `ingress ls`) tem de os actualizar.
  Um nó com o pin antigo precisa de `netns down`/`up` (reinicia os workloads) para ter v6.
- Dívida assumida: sem redes só-v6; sem publicação de portas em v6 (slirp4netns 1.2.1); saída v6
  dependente do host; VMs depois dos containers; o overlay VXLAN/WireGuard entre nós fica v4 (o
  túnel); os `allowed-ips` v6 por cima dele não estão neste ADR.
- O `br_netfilter` passa de recomendação a pré-condição dura para uma rede `--ipv6` (D3).
- Matriz: `net.ipv6` na coluna `linux` passa a `partial` quando P1–P3 estiverem fundidas (com a
  evidência dos checks 1–6) e a `supported` com o portão D10 inteiro; `net.nat.npt` continua
  `unsupported-by-provider` (NAT66 por masquerade não é NPTv6), com a razão actualizada.

## Plano de implementação

| Fase | O que entra | Ficheiros | Prova |
|---|---|---|---|
| **P0** | Anti-spoof v4 para `table bridge dlxspoof` (MAC + ARP + `ip saddr`), nos três caminhos de attach; o `antispoof_rule_args` do `fwdeny` sai | `delonix-sdn/src/infra.rs` (`antispoof_rule_args`, `clear_antispoof`, `do_attach`, `do_attach_extra`, `do_vmtap`, `ingress_table_ruleset`) | check novo na bateria: origem v4 forjada não chega a vizinho nem ao holder; verificado vermelho com o código de hoje (S3) |
| **P1** | `table inet dlxing` com `fwmap4`/`fwmap6`, `dlxall4/6`, `dlxns4/6`; v6 ainda desligado; recusa de reattach a pin `ip` para redes v6 | `infra.rs` (59 sítios de `INGRESS_TABLE`, `ingress_table_ruleset`, `fw_dispatch_*`, `ns_set_join/leave`, `parse_fwmap_elements`), `policy_nft.rs` (`chain_body` emite `ip` e `ip6`) | a bateria inteira de rede v4 sem mudança de resultado; testes do corpo da chain com as duas famílias |
| **P2** | Global ID, `NetDef.ipv6_prefix`, `network create --ipv6`/`--ipv6-subnet`, `spec.ipv6`; derivação v4→v6 pura | `delonix-net-rules` (`Cidr6`, derivação), `delonix-sdn/src/infra.rs` (`NetDef`), `ipam.rs` (Global ID), `bins/delonix-runtime-bin/src/cmd/network.rs`, `delonix-stack` (campo comparado) | testes puros da derivação e da recusa de sobreposição/`fd00::/64`; `stack plan` sem deriva |
| **P3** | Verbo `addr6`, `control_ipv6_ok`, `accept_ra=0`, anti-spoof v6/RA/NA na `dlxspoof`, `bridge-nf` posto e verificado (D3), fim do RA e do `fd00:` incondicional | `infra.rs` (`validate_control_tokens`, `handle_control`, `do_attach*`, `ensure_net_bridge`, `ra_sender_main`), `cmd/container.rs` (rollback do attach) | checks 1–8, 10–12 |
| **P4** | DNS `AAAA` (nomes e `Service`) | `infra.rs` (`dns_action_owned`, `handle_dns`, `build_dns_index`, `dns_resolve*`) | check 9 |
| **P5** | Saída: slirp `--enable-ipv6`, `accept_ra=2` no `tap0`, NAT66, `fwguard` v6; VMs CH com `vmtap6` e v6 no `network-config` | `infra.rs` (spawn do slirp, `base ruleset`, `vmtap_line`), `delonix-vm`/`cmd/vm.rs` (seed) | saída v6 num host com v6 (o laboratório D1 do plano de maturidade); VM com v6 e anti-spoof |
| **P6** | Portão D10 completo + cenário de caos; matriz `net.ipv6` → `supported`; `DELONIX_ENABLE_IPV6` removido; doc (`AGENTS.md`, `docs/gen.py`) | `scripts/e2e.sh`, `scripts/chaos.sh`, `crates/adapters/delonix-sdn/src/provider_report.rs`, `docs/providers/capability-matrix.md` | o portão inteiro verde numa release; só depois se discute a omissão (D8) |

Cada fase passa pela `delonix-runtime-sec` antes de fundir (guarda-rio 5): P0, P1 e P3 mexem em
fronteiras de isolamento.

## Questões para o dono

1. **O anti-spoof v4 inerte (S3) sai já, à parte?** É um defeito de segurança em produção
   (falsificação de origem v4 e, pelas VMs, do `tap`) que não depende de IPv6. Recomendação: sim,
   como P0 independente, com a sua auditoria.
2. **Opt-in agora, omissão depois (D8)** — confirma que «a recusa actual passa a opt-out» do plano
   de maturidade fica para depois do portão, numa major?
3. **Só dual-stack (D4)** — aceitável sem redes só-v6?
4. **Publicação de portas v6 fora** (slirp4netns 1.2.1 não suporta) — aceitável, ou investiga-se
   outra via (um slirp mais recente, `pasta`)? Seria outro ADR.
5. **Saída v6** só pode ser provada num host com v6 — o laboratório nocturno da D1 do plano tem
   v6 de saída?
6. **Reattach a um pin antigo recusa v6** e o remédio reinicia os workloads — aceitável como custo
   único do upgrade?
