# Revisão de rede, DNS e isolamento (2026-08-11)

Auditoria pedida sobre: bugs, latências, gaps, colisão de DNS e boas práticas
cloud-native de rede e isolamento (namespace, stack, pod, default).

**Método**: nó Delonix completo montado à parte (`DELONIX_ROOT=/tmp/dxa/root`,
`DELONIX_NET_RUNTIME_DIR=/tmp/dxa/run`), com pin/controlo/slirp próprios. O nó
real deste host tem trabalho vivo (`kaeso-odoo18-multi`, `kaeso-db18`) e não foi
tocado — os PIDs foram confirmados inalterados no fim. **Tudo o que se segue foi
medido, não deduzido**; onde não houve medição, está dito.

---

> **Estado (2026-08-12).** Fechados e validados ao vivo: **A1, A2, A4, A5**
> (1.ª passagem) e **A3, A6, A7, A8, A10** (2.ª passagem, sob ADR-0011). **A9
> fecha por decisão consciente** — o `default` continua aberto, e a razão está
> no ADR §5. Um achado **novo (A11)** apareceu ao validar, é PRÉ-EXISTENTE e
> fica aberto com a medição feita. Esta nota é mantida a par das correcções de
> propósito — uma tabela de achados que não acompanha o que foi fechado mente
> nos dois sentidos, como o `AUDITORIA-E2E.md` fez durante semanas.

## A1 — CRÍTICO ✅ CORRIGIDO. A descoberta de serviço está partida para aplicações reais

Um container **não consegue falar com outro pelo nome** se usar `getaddrinfo()`
— o caminho por omissão de Go, Java, Node, Python, curl, wget, nc. Medido:

```
nslookup -type=a weba   → 10.250.209.43     ✓
getent hosts weba       → 10.250.209.43     ✓
nc -w2 weba 9           → nc: bad address 'weba'      ✗
wget -O- http://weba:8080/ → wget: bad address 'weba:8080'   ✗
```

**Causa**, isolada por query directa ao servidor:

```
weba A     → NOERROR  10.250.209.43   (nosso)
weba AAAA  → SERVFAIL                 (veio do UPSTREAM)
```

`handle_dns` só trata `qtype == 1` (A). Tudo o resto — incluindo o AAAA que
`getaddrinfo` emite **sempre**, em paralelo com o A — cai em `forward_dns` e vai
ao resolvedor externo, que não sabe nada de um nome de uma só etiqueta e devolve
SERVFAIL. musl e glibc tratam SERVFAIL numa das metades como falha da resolução
inteira.

Só funcionam as ferramentas que pedem A explicitamente (`ping` do busybox,
`getent`) — que é exactamente porque isto pode passar despercebido em testes
manuais e falhar em todas as aplicações.

**Correcção**: para um nome que o índice resolve, responder ao AAAA com **NODATA
autoritativo** (rcode 0, ANCOUNT 0). É a resposta correcta — o IPv6 está
desligado por desenho desde a v0.37.1 — e é o que o CoreDNS faz.

## A2 — CRÍTICO ✅ CORRIGIDO. Nomes internos vão para fora, e não há resposta negativa própria

O servidor **nunca gera uma resposta negativa**. Confirmado por varredura: não
existe no ficheiro nenhuma construção de NXDOMAIN/SERVFAIL. Portanto todo o
`.delonix.internal` que não resolve é reencaminhado:

```
weba.teamA.delonix.internal AAAA → NXDOMAIN, vindo do upstream
client.teamB.delonix.internal A  → NXDOMAIN, vindo do upstream
```

Duas consequências:

1. **Fuga de informação.** Os nomes dos workloads e das namespaces de cada
   inquilino são enviados para o resolvedor externo (`SLIRP_DNS`, depois
   `1.1.1.1`). Um domínio interno nunca deve sair do nó.
2. **Latência e DoS.** Cada nome interno que não resolve custa até **6 s** (2
   upstreams × 3 s) e prende uma thread do tecto de 64. Medido com o upstream
   inacessível: uma query AAAA de um container **vivo** custou **9,03 s**, e
   `nslookup weba` (que emite A+AAAA) demorou **5,05 s** terminando em
   `*** Can't find weba: No answer`.

O CLAUDE.md afirma que uma namespace errada «dá NXDOMAIN» — dá, mas o NXDOMAIN é
do upstream, não nosso. O comportamento observável coincide; o custo e a fuga não.

**Correcção**: NXDOMAIN autoritativo para qualquer `.delonix.internal` que o
índice não conheça, sem nunca reencaminhar esse sufixo.

## A3 — CRÍTICO ✅ CORRIGIDO (ADR-0011). O DNS ignora a namespace de quem pergunta

O dataplane isola (medido: `client`@teamA → `webb`@teamB = 100 % de perda), mas o
plano de nomes não isola nada:

```
client(teamA) → nslookup webb                        → 10.250.198.79
client(teamA) → nslookup webb.teamB.delonix.internal → 10.250.198.79
```

Um inquilino enumera a existência e o endereço exacto de todos os workloads dos
outros. `dns_server_main` **tem** o IP de origem (`peer`) e deita-o fora —
`handle_dns(&q)` não o recebe.

Isto torna também o desempate de nomes nus (`idx.entry(name).or_insert(ip)`,
«first wins») estruturalmente indecidível: a resposta certa depende de quem
pergunta.

**Correcção**: passar `peer.ip()` a `handle_dns`, mapear origem→namespace pelo
índice que já existe, e resolver o nome nu **primeiro dentro da namespace do
requerente**. É o modelo do `search <ns>.svc.cluster.local` do k8s.

## A4 — GRAVE ✅ CORRIGIDO. Membro de pod em rede custom regista o IP ERRADO → colisão real

Medido, com dois pods:

| pod | rede | IP real | `ip` no registo |
|---|---|---|---|
| `p1` | `audit` (10.250/16) | **10.250.0.2** | `10.200.0.2` |
| `pdef` | default (10.200/16) | 10.200.0.2 | `10.200.0.2` |

Os dois pods ficam com **o mesmo endereço no registo**, e esse endereço pertence
de facto ao `pdef`. Consequência medida: `nslookup p1-a` devolve `10.200.0.2` —
**o outro pod**. Tráfego endereçado por nome a um pod é entregue a outro, que
pode ser de outro inquilino. Controlo pelo IP real confirma que os workloads
estão bem (`nc 10.250.0.2 8080` → `SOU-O-P1`); o que está errado é o nome.

**Causa-raiz** (`cmd/container.rs:3330`):

```rust
let ip = infra::container_ip(pn);   // "IP on the DEFAULT ingress network (10.200.A.B)"
```

`container_ip()` calcula sempre no prefixo default e o caminho do pod chama-a sem
olhar à rede do pod. **A função certa existe e não é chamada**:
`container_ip_on(prefix, id)`. Sexta ocorrência do padrão já catalogado neste
repo (`mount_live`, `set_net_rate`, `update_limits`, `create_with_base`,
`JsonStore::update`).

O `fwmap` está correcto (10.250.0.2 presente) porque
`apply_pod_namespace_isolation` usa o nome da netns e não o `c.ip` do membro —
por isso o isolamento aguenta e o dano fica no DNS. Verificado: `spy`@teamB →
pod@teamA = 100 % de perda; `client`@teamA → pod = 0 % de perda.

## A5 — MÉDIO (segurança) ✅ CORRIGIDO. `forward_dns` aceita a resposta de qualquer origem

```rust
let sock = UdpSocket::bind("0.0.0.0:0")?;   // sem connect()
sock.send_to(q, "10.0.2.3:53");
if let Ok((n, _)) = sock.recv_from(&mut buf) { return Some(buf[..n].to_vec()) }
```

A origem é descartada (`_`), não há `connect()`, e nem o txid nem a pergunta são
conferidos contra o que se enviou. A primeira resposta UDP que chegue à porta
efémera é entregue ao cliente. É o vector Kaminsky clássico. O anti-spoofing da
bridge dificulta-o mas não o fecha, e a correcção é uma linha: `connect()` no
socket faz o kernel recusar tudo o que não venha do upstream.

## A6 — GRAVE ✅ CORRIGIDO (ADR-0011). Nomes de container são GLOBAIS, não por-namespace

```
run --name web --namespace teamA   → ok
run --name web --namespace teamB   → error: the name 'web' is already in use
```

Uma namespace que não é um espaço de nomes contradiz o próprio nome e o modelo
k8s, onde a unicidade é *dentro* da namespace. Impede duas equipas ou duas stacks
de terem ambas um `db`/`web`/`api` — precisamente os nomes que toda a gente usa —
e força prefixos manuais (`loja-api`, `banco-api`) que reinventam à mão a
namespace que já foi declarada.

## A7 — GAP ✅ FECHADO (aviso; ver ADR-0011 §4 para o que NÃO se fez). `stack` não é fronteira de isolamento nenhuma

Duas stacks distintas, sem `namespace` declarada, caem ambas em `default` e
alcançam-se e resolvem-se sem barreira (medido: 0 % de perda, nome resolvido):

```
loja-api   ns=default  {'delonix.io/stack': 'loja'}
banco-api  ns=default  {'delonix.io/stack': 'banco'}
```

A label `delonix.io/stack` é posse para o reconciliador, não política de rede.
Não é bug — nada promete o contrário — mas é o gap que mais surpreende quem chega
do compose (onde um projecto tem rede própria por omissão). As opções são
derivar a namespace do nome da stack quando não é declarada, ou documentar
explicitamente que isolar é declarar `namespace`.

## A8 — GAP ✅ CORRIGIDO. Um Pod não tem nome DNS

`nslookup p1` → SERVFAIL. Só os membros (`p1-a`, `p1-b`) resolvem — e resolvem
para o IP errado (A4). O k8s dá nome ao Pod e, sobretudo, um **Service** com nome
estável à frente de N réplicas. Sem isso não há descoberta de serviço estável:
o consumidor tem de conhecer o nome de um membro concreto.

## A9 — MENOR ✅ FECHADO POR DECISÃO (ADR-0011 §5). O `default` é assimétrico, e é o oposto do default seguro

Já documentado como limitação v1: `default` é alcançável de dentro de qualquer
namespace mas não alcança para dentro delas. A prática cloud-native (k8s
NetworkPolicy, e o que qualquer auditoria de segurança pede) é **default-deny**
com abertura explícita. Hoje quem não declara `namespace` fica numa SDN plana e
aberta, que é o caso por omissão — portanto o estado por omissão é o menos
seguro. Mudar isto é breaking e merece decisão própria (ADR).

## A10 — MENOR ✅ CORRIGIDO (stderr; o MTU fica). Observabilidade e MTU

- O stderr do processo de controlo vai para `/dev/null` (confirmado em
  `/proc/<pid>/fd/2`). Um pânico numa thread de DNS é invisível: o serviço
  degrada e não há uma linha em lado nenhum. Foi o que tornou o diagnóstico de
  A1/A2 mais lento do que precisava.
- MTU: `delonix0` e os veth a 1500, `tap0` a 65520. Sem problema hoje, mas o
  overlay VXLAN encapsula (−50 B) e não há ajuste de MTU em lado nenhum — é a
  causa clássica de «o TCP pendura em transferências grandes» quando o
  inter-nó entrar ao barulho.

---

## O que está bem, e foi medido

Vale registar, para a revisão seguinte não voltar a suspeitar daqui:

- **Isolamento cross-namespace de containers e de pods**: correcto nos dois
  sentidos. `fwmap` com uma entrada por workload e o IP certo.
- **Concorrência do socket de controlo**: 12 attaches concorrentes em **1091 ms**
  (~90 ms cada), 12/12 sem falha. A correcção do tecto de 30 s aguenta.
- **Latência de criação** de container em rede custom: 249–341 ms.
- **Resolução A de nome interno**: 0 ms no servidor, 19–38 ms fim-a-fim.
- **DNS não se pendura no arranque**: o servidor respondeu em 0 ms mesmo com
  threads presas em forwards lentos.

## Ordem sugerida

1. **A1 + A2 juntos** — são a mesma correcção (tratar AAAA e o sufixo interno
   localmente, com resposta autoritativa). Desbloqueiam a descoberta de serviço,
   fecham a fuga de nomes e eliminam a via de DoS. É a correcção de maior
   retorno de toda a lista.
2. **A4** — uma linha (`container_ip_on` com o prefixo da rede do pod) e um
   teste que exija dois pods em redes diferentes.
3. **A5** — uma linha (`connect()`).
4. **A3** — precisa de desenho (o `peer` tem de descer até ao resolvedor).
5. **A6/A7/A9** — são decisões de modelo, não bugs; merecem ADR antes de código.

---

## Validação das correcções (A1, A2, A4, A5)

Nó novo montado com o binário corrigido (`/tmp/dxb`, pin/controlo/slirp
próprios), mesmo cenário que produziu cada achado.

**A1 — descoberta de serviço.** O caminho `getaddrinfo`, que era o partido:

```
antes:  wget -O- http://weba:8080/  → wget: bad address 'weba:8080'
depois: wget -O- http://weba:8080/  → Connecting to weba:8080 (10.250.9.124:8080)
antes:  nc -w2 weba 9               → nc: bad address 'weba'
depois: nc -w2 weba 9               → (liga)
```

**A1/A2 — no protocolo.** Ambas as respostas passam a ser nossas, com `aa`:

```
weba AAAA                        → NOERROR, aa, ANSWER: 0, Query time 0 msec   (era SERVFAIL do upstream)
naoexiste.teamA.delonix.internal → NXDOMAIN, aa,           Query time 0 msec   (era NXDOMAIN do upstream)
```

**A2 — a prova de que já não sai do nó.** Com o upstream DROPADO por nftables
(o caso air-gapped, e o que expunha o custo):

| | antes | depois |
|---|---|---|
| `nslookup weba` (A+AAAA) | **5,047 s**, `*** Can't find weba: No answer` | **0,034 s**, endereço devolvido |
| `wget http://weba:8080/` | `bad address` | **0,040 s**, resolve e liga |
| AAAA de container vivo | **9,03 s** | 0 ms |

~148× no caminho de resolução, e a fuga fecha por construção: uma resposta em
0 ms com a saída bloqueada não pode ter ido a lado nenhum.

**Não parti a internet** — `example.com` continua a ser encaminhado e resolve
(`172.66.147.243`), e `.delonix.io` continua deliberadamente fora da nossa zona.

**A4 — colisão eliminada.** Mesmos dois pods de antes:

```
antes:  p1-a 10.200.0.2 · p1-b 10.200.0.2 · pdef-a 10.200.0.2   (os três iguais)
depois: p1-a 10.250.0.2 · p1-b 10.250.0.2 · pdef-a 10.200.0.2   (cada um o seu)
```

E com tráfego real, as duas correcções juntas: `nc -w3 p1-a 8080` a partir de
outro container resolve por nome **e** chega ao pod certo — `SOU-O-P1`. Antes
falhava em `bad address`, e o nome apontava para o outro pod.

**Sem regressão de isolamento** (o que mais importava não partir): `spy`@teamB →
`weba`@teamA e → pod@teamA continuam a 100 % de perda; `client`@teamA → `weba` a
0 %. `cargo build`/`clippy`/`fmt` limpos e a suite completa do workspace a
passar; os dois testes de decisão do DNS **falham com a correcção revertida**
(verificado explicitamente, conforme a regra do repo).

---

## Validação da 2.ª passagem (A3, A6, A7, A8, A10 — sob ADR-0011)

Nó novo (`/tmp/dxd`), com o cenário que a auditoria não conseguia montar antes:
**dois inquilinos com o MESMO nome**, coisa que a unicidade global proibia.

**A6 — dois `db` coexistem** (o segundo era recusado), e cada inquilino resolve
o seu:

```
db  ns=teamA  ip=10.250.229.6
db  ns=teamB  ip=10.250.194.118

app(teamA) → nslookup db  → 10.250.229.6     (o seu)
app(teamB) → nslookup db  → 10.250.194.118   (o seu)
```

**A3 — a fuga fechou**, e fechou com precisão de endereço e não de namespace:

```
app(teamA)   → db.teamB.delonix.internal   → NXDOMAIN
  (depois de `ingress allow teamB/db 5432 --from <ip do app>`)
app(teamA)   → db.teamB.delonix.internal   → 10.250.194.118   ← a Dependency abre
outro(teamA) → db.teamB.delonix.internal   → NXDOMAIN         ← e SÓ para o endereço nomeado
```

O vizinho da MESMA namespace, não coberto pelo allow, continua sem ver — é a
diferença entre espelhar a política e abrir a fronteira.

**A6 no caminho destrutivo**, que é onde adivinhar custa caro:

```
net ingress allow db 5432 …        → recusa: «exists in several namespaces
                                      (teamA/db, teamB/db) — qualify it»
net ingress allow teamB/db 5432 …  → aplica, no db certo
```

Isto veio de um gap que a própria mudança criou e que só a validação ao vivo
mostrou: `util::find` passou a recusar nomes ambíguos, mas `Store::load` — por
onde o firewall e os manifestos resolvem — continuava com «o mais recente
ganha». Um `ingress deny db` teria escolhido um inquilino em silêncio. Corrigido
no `Store::load`, com teste.

**A8 — o pod resolve pelo próprio nome**: `nslookup web` → `10.250.0.2` (dava
SERVFAIL). **A10** — o log do controlo existe e foi USADO nesta sessão para
excluir um pânico em segundos, que era exactamente o que faltava na 1.ª
passagem.

**Sem regressão**: externos resolvem (`example.com` → 104.20.23.154);
`getaddrinfo` por nome funciona (`wget http://db:5432/` → conecta a
`10.250.229.6`); 20 resoluções seguidas em **68 ms**.

## A11 — ABERTO, PRÉ-EXISTENTE. A 1.ª resolução após criar um pod falha, às vezes

Encontrado a validar. Logo depois de um `pod create`, a primeira query DNS de um
container **já existente** pode dar timeout (5,04 s); as seguintes respondem em
~30 ms. A rede não está partida (ping ao gateway a 0,127 ms) e o servidor está
vivo (um `dig` de dentro do holder responde em 1 ms na mesma janela) — o que
falha é o pedido do container.

**Medido nos dois binários, com o MESMO cenário**, porque a primeira leitura foi
«a regressão é minha» a partir de uma amostra de UM, que é o erro de método que
este ficheiro já documenta noutro sítio:

| binário | falhas |
|---|---|
| com estas correcções | **0 / 10** |
| `main` (sem elas) | **2 / 10** |

Portanto é pré-existente e intermitente, e não há evidência de que este trabalho
o agrave. Ver **D4** abaixo para o que se fez a seguir.

---

# 3.ª passagem: fechar o DNS para produção crítica (2026-08-12)

Pedido: «tudo precisa resolver por nome». Inventário MEDIDO do que resolvia,
feito antes de mexer em nada — porque até aqui «o que falta» era suposição.

| forma | antes | agora |
|---|---|---|
| `api` (nome nu) | ✓ | ✓ |
| `api.loja.delonix.internal` | ✓ | ✓ |
| `api.delonix.io` (legado) | ✓ | ✓ |
| **alias de rede** (`backend`) | **✗** | ✓ |
| **`api.loja`** (curta) | **✗** | ✓ |
| **workload MORTO** | **resolvia** ✗ | NXDOMAIN ✓ |
| externo (`example.com`) | ✓ | ✓ |

## D1 — um workload morto continuava a resolver

Medido: um container que sai sozinho fica `status=Stopped` com o endereço ainda
no registo, e o `nslookup` continuava a entregá-lo. Um cliente ligava-se a um
serviço que já não existe e esperava por um timeout, em vez de falhar depressa
com NXDOMAIN. E o lease volta ao IPAM, por isso o nome de um serviço morto pode
passar a apontar para o workload que receba esse endereço a seguir.

**Não consegui provocar a reatribuição** em 6 tentativas — os endereços derivam
do id e a colisão a curto prazo é improvável. Fica dito, porque a consequência
grave é essa e não a demonstrei.

O índice passa a servir só `Running`/`Paused`/`Created`. `Failed(code)` e o
legado `Exited` serializam como OBJECTOS e chegam como `None`, tal como um
registo sem estado — os três param de responder. Fail-closed, que é o lado certo
da pergunta «este endereço ainda é teu?». O mesmo se aplica a VMs paradas.

## D2 — os aliases de rede nunca foram consultados

`--network-alias`/`networkAlias:` era persistido em `net_aliases` desde sempre e
**nada o lia** — a opção era aceite e não fazia nada, a forma de falha que este
repo persegue. Passam a ser indexados como nomes, com o mesmo âmbito de
namespace e com `or_insert`, para que um alias nunca desloque um container que
seja mesmo dono daquele nome. Validado: `backend` e `api-v2` → o endereço do `api`.

## D3 — a forma curta `<nome>.<namespace>`

`api.loja` não resolvia; só o nome nu e o FQDN inteiro. O k8s dá isto com
`search` no `resolv.conf`; aqui faz-se no resolvedor de propósito — um `search`
faz o cliente colar o nosso sufixo a **todas** as falhas externas, o que é
tráfego e fuga por omissão.

Só é tentada quando a string inteira não resolve como nome próprio, e **a
namespace tem de existir neste nó** para o nome ser nosso: sem isso, um
`api.loja` que não resolvesse saía com a namespace do inquilino colada. Um nome
com mais de uma etiqueta depois da cabeça (`api.github.com`) nunca é nosso — e
um container numa namespace chamada `com` não nos faz reclamar `github.com`.
Validado: `naoexiste.loja` → **NXDOMAIN autoritativo (`aa`) em 1 ms**.

## D4 — o A11, e o que se fez sem o corrigir às cegas

Não reproduziu com o código actual na forma sequencial (0/10). Reproduziu com
uma sonda CONTÍNUA de dentro de um container enquanto se criavam pods: **2 em
300** (~0,7 %), contra 0 em 90 sem criação nenhuma. Não isolei a causa, e uma
correcção sem causa isolada é um palpite.

**Nota de método**: a primeira sonda corria `dig` a partir do holder e deu
200/200 «falhas» — o holder não pertence à namespace, logo o âmbito recusava-o
correctamente. Era o isolamento a funcionar, não uma avaria. Uma sonda tem de
correr de onde o cliente real corre.

O que se fez é outra coisa e é defensável por si: **`options timeout:2
attempts:3` no `resolv.conf`**. O DNS é UDP e um datagrama perdido enquanto o nó
reprograma o dataplane é um evento NORMAL; o default da libc é `timeout:5
attempts:2`, ou seja **cinco segundos** até à primeira retentativa — é isso que
transforma uma perda transitória numa paragem visível. Medido depois, no mesmo
cenário e com 16 pods criados durante as sondas: **0 falhas em 600**.

Isto não afirma que a causa foi corrigida — afirma que o cliente deixou de a
sentir. A causa fica aberta, com o número.
