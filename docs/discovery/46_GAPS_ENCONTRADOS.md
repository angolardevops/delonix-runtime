# 46 — Discovery: fundação de rede/namespaces (bloco A) e dados (bloco B)

| Campo | Valor |
|---|---|
| Data | 2026-08-10 |
| Linha de base do prompt | `0.45.0` (commit `544060a`) |
| **Linha de base real no `main`** | **`0.46.0`** (commit `801deee`) — uma versão à frente do prompt |
| **Binário que serve o holder vivo** | **`0.44.0`** (commit `663c9b6`, `/usr/local/bin/delonix`) |
| Método | Medição ao vivo num host rootless real + leitura de código |
| Alterações de produção | Nenhuma. Este documento é o único artefacto. |

> **Aviso de leitura.** Tudo o que segue foi medido ou lido. Onde não foi possível
> medir, diz `NÃO MEDIDO` e porquê. Nenhuma classificação é inferida da documentação
> — houve pelo menos um caso (§4.1) em que a documentação afirma o contrário do
> comportamento real.

---

## 0. Condições da medição (o que valida e o que não valida)

### 0.1 O holder vivo é duas versões mais antigo que o `main`

```
$ /usr/local/bin/delonix --version   → 0.44.0 (663c9b6)   ← arrancou o pin/controlo vivos
$ grep version Cargo.toml            → 0.46.0
$ git diff --numstat v0.44.0..HEAD -- crates/delonix-net
21   7   crates/delonix-net/src/bpf.rs
172  41  crates/delonix-net/src/infra.rs
```

O dataplane **mudou** entre o holder vivo e o `main`. Os dois commits são `d5d6553`
(segundo state root) e `801deee` (auditoria v0.46.0). O que muda em `infra.rs`:
`foreign_holder_message`, `antispoof_rule_args` (partilhado veth/tap), o antispoof no
`do_vmtap`, e a assinatura de `reap_orphan_hostfwds`.

**Consequência para este discovery**, aplicada caminho a caminho:

- as chains de namespace/firewall por-container **não mudaram** → as medições de §2 e §3
  são válidas para o `main`;
- o **antispoof do tap de VM** existe no `main` e **não** no holder vivo → um teste aqui
  mediria um gap que o `main` já fechou. Não é reportado como gap.

O host tem 5 containers de produção (Odoo/Postgres) vivos. **Não foi respawnado o
holder** — por isso nenhuma correcção ao código do holder é validável ao vivo nesta
sessão, só por teste. É a mesma restrição já registada no `AGENTS.md`.

### 0.2 Banco de ensaio

Quatro containers `alpine:latest` na SDN existente (`kaeso-net`), em três namespaces, mais
um pod. Servidor `nc -l -p 8080`; cliente `nc`/`wget`. Todos removidos no fim (§6).

> **Armadilha de método, custou duas medições erradas.** `nc` a falar HTTP com
> `tail -1` devolve vazio tanto para *bloqueado* como para *404 sem corpo* — cheguei a
> registar «bloqueado» para um caminho que afinal **passa**. Só um controlo explícito
> (o mesmo pedido a partir do host, que respondeu `WIN`) separou as duas coisas, e a
> conclusão inverteu-se. Toda a medição de alcançabilidade neste documento tem controlo
> positivo; onde o veredicto é «bloqueado», existe uma linha ao lado que prova que o
> mesmo aparato alcança quando deve.

---

## 1. Matriz de alcançabilidade (A1)

Medida com política e namespaces reais. `REACHABLE`/`blocked` são output de comando, não
expectativa.

| # | Caminho de dados | Medição | Veredicto |
|---|---|---|---|
| 1 | IPv4 primário, cross-namespace | teamA→teamB `blocked`; teamB→teamA `blocked`; default→teamA `blocked`; teamA→default `REACHABLE` | ✅ correcto (assimetria documentada) |
| 2 | Multi-homing (IP de rede extra) | cross-ns `blocked` pelo primário **e** pelo extra; `@fwmap` mapeia os 2 IPs para a **mesma** chain | ✅ coberto |
| 3 | IPv6 / ULA | `disable_ipv6=1`, 0 endereços `inet6`; `table ip6 dlxing` `policy drop` com `counter 241 packets` | ✅ fechado |
| 4 | Porta publicada (`-p`) | `allow`→`OK`; `policy deny`→vazio; `+allow 8080`→`OK` | ✅ governado |
| 5 | `proto: any` + porta nua | `allow 9999` **não** abre a 8080; `allow 8080` abre | ✅ correcto — **provado ao vivo** (era só teste unitário) |
| 6 | Saída do próprio container sob `policy deny` | `REACHABLE` (o `ct state` da v0.37.x) | ✅ correcto |
| 7 | `--net host` / `none` | `ingress ls` diz `n/a (host net)` | ✅ honesto |
| 8 | Proxy L7 (`httproute` / `--expose`) | era: teamB→proxy→teamA `WIN` mesmo com `policy deny`. Agora: `timeout`, host→proxy `WIN` (§4.2, corrigido) | ✅ fechado |
| 9 | netns de pod | cross-namespace `blocked` na rede custom; `spec.network` era ignorado (§4.4, corrigido) | ✅ coberto |
| 10 | `tap` de VM | **NÃO MEDIDO** — nenhuma imagem deste host arranca em Cloud Hypervisor (a golden é libvirt-only). O antispoof existe no `main` (`antispoof_rule_args`), ausente no holder vivo | — |
| 11 | `vm bridge` (privilegiado) | **NÃO MEDIDO** — exige `root` e um veth no init-netns do host; não executado num host com produção viva | — |
| 12 | Storage de rede (NFS/CIFS) | **NÃO MEDIDO** — `mount -t nfs` exige `CAP_SYS_ADMIN`, indisponível em rootless puro | — |

Precedência das chains, lida do kernel (`nft list ruleset` dentro do netns do pin), toda
no hook `forward` — a comparação de prioridades só é válida dentro do mesmo hook:

```
fwguard  priority -20   ip daddr 169.254.0.0/16 drop ; ip daddr 127.0.0.0/8 drop
fwdeny   priority -10   antispoof por veth (8 veths, 8 regras — 1:1)
fwcont   priority  -5   ip daddr vmap @fwmap ; ip saddr vmap @fwmap
forward  priority   0   policy drop; ct established,related accept; …
```

---

## 2. O que está certo e foi confirmado (não mexer)

Registado porque metade do valor de uma auditoria é saber o que **não** precisa de trabalho:

- **O CRÍTICO da v0.37.0 (`proto: any` a descartar a porta) está fechado, agora provado ao
  vivo.** O `AGENTS.md` dava-o como «provado por teste unitário, NÃO ao vivo» porque o
  holder não podia ser respawnado. O holder actual já tem o código; a medição de §1/#5
  fecha essa pendência documental.
- **Multi-homing está coberto de facto**, não só por leitura: `@fwmap` tem
  `10.210.51.70 : jump fw237dd714` **e** `10.233.51.70 : jump fw237dd714` — o mesmo
  destino para os dois IPs do mesmo container.
- **`pod rm` sem `-f` é fail-closed exemplar**: recusa, nomeia o membro que está a correr e
  imprime o comando de recuperação.
- **`net ingress ls`** já distingue `n/a (host net)` de `allow (default)`.

---

## 3. Achados — bloco A

### 4.1 ALTO — `container rm` nunca desregista a auto-rota do proxy L7

> A documentação afirma o contrário: *«auto-registado … removido no `container rm`»*
> (`AGENTS.md`, secção do reverse-proxy).

**Reprodução:**

```
$ delonix container run -d --name d46-x --net kaeso-net --expose 8080 alpine:latest sleep 300
$ grep -o '"expose":[^,]*' ~/.local/share/delonix/containers/*.json | grep -v null
(nada)                                   ← o campo NUNCA é persistido
$ delonix container rm -f d46-x
$ cat ~/.local/share/delonix/httproute/auto.json
[ { "name": "d46-a", "namespace": "teamA", "ip": "10.210.23.139", "port": 8080 },
  { "name": "d46-x", "namespace": "default", "ip": "10.210.15.167", "port": 8080 } ]
```

Os dois containers **já não existem** (`container ls -a | grep -c d46-a` → `0`).

**Causa-raiz.** O caminho de remoção existe e está correcto —
`ingress_proxy::auto_deregister` (`ingress_proxy.rs:751`), chamado em
`container.rs:4521`. O que falha é a **guarda**:

```rust
// container.rs:4520
if c.expose.is_some() {
    super::ingress_proxy::auto_deregister(&c.name);
}
```

`c.expose` é atribuído em `container.rs:2977`, mas o `store.save` desse ramo não chega a
gravar o campo em disco — nenhum registo de container tem `expose` não-nulo. A guarda é
portanto **sempre falsa** e o `auto_deregister` é, na prática, código morto.

**É a 5.ª ocorrência da armadilha já documentada quatro vezes** («estado necessário para
RECONSTRUIR o recurso tem de ser persistido, não só usado na criação» — `-v`, `-p` em rede
custom, redes extra, `Container.pod`). Aqui não custa uma reconstrução: custa uma rota que
sobrevive ao seu dono.

**Consequência de segurança.** A rota fica a apontar para um IP da SDN que o IPAM
**reatribui**. O container seguinte a receber `10.210.23.139` passa a receber, em silêncio,
o tráfego endereçado a `d46-a.teamA.delonix.internal` — de outra namespace e de outro dono.
Combinado com §4.2 (qualquer container alcança o proxy), é um caminho de entrega cruzada
entre inquilinos.

**Agravante operacional:** não há comando para limpar isto. `httproute rm` só toca na parte
MANUAL — por desenho — e `auto_deregister` não tem superfície de CLI. A recuperação nesta
sessão foi editar `auto.json` à mão.

**Estado: consequência CORRIGIDA, causa-raiz AINDA ABERTA.**

A guarda foi removida — `auto_deregister(&c.name)` corre agora sempre no `rm`. É seguro e
barato: `with_auto_locked` sai sem escrever (e sem recompor o proxy) quando a lista não
muda, e o contrato da própria função já é «se não estava registado, não faz nada». A
limpeza passa a depender do **nome**, que temos sempre, em vez de um campo do registo.

Validado ao vivo, mesmo comando antes e depois:

```
antes (com a guarda)  rm de d46-a/d46-x/d46-y → auto.json ficou com as 3 rotas órfãs
depois (sem a guarda) rm de d46-fix           → auto.json vazio, proxy parado
```

Sem teste unitário no ponto de chamada, e a razão é concreta: `with_auto_locked` chama
`rebuild()`, que fala com o holder e arranca/pára o proxy — um teste do `cargo test` teria
efeitos colaterais no nó. A prova é a validação ao vivo acima.

**Por resolver: porque é que `expose` não chega ao disco.** Eliminado nesta sessão, para
não ser repetido:

- o spec do re-exec **leva** o valor — apanhado em voo:
  `.reexec-<id>.json` → `expose = 8080`;
- `run_from_spec` (`container.rs:3813`) desserializa o `RunOpts` e passa-o **inteiro** ao
  `cmd_run` — não há reconstrução campo-a-campo a perdê-lo;
- `Container::expose` não tem `skip_serializing` (só `#[serde(default)]`), logo seria
  escrito se estivesse presente;
- não há nenhum `store.save` em `container.rs` depois da linha 2979 no caminho do `run`;
- a 2.ª passagem **chega** ao bloco que o grava — prova-o o `c.ip` da linha 2952, que fica
  correcto no registo.

Resta portanto explicar como a variável local `expose` chega a `None` na 2.ª passagem
apesar do spec a trazer. O sintoma persiste com a correcção acima aplicada (que não lhe
toca) e continua a ser observável em uma linha:
`delonix container run -d --net <rede> --expose 8080 …` seguido de ver `expose` no registo.

---

### 4.2 ALTO — o proxy L7 fura o isolamento de namespace **e** a política por-container

**Reprodução** (backend `d46-a` em `teamA`, cliente `d46-b` em `teamB`):

```
teamB → teamA directo ................ wget: download timed out   [bloqueado ✓]
teamB → proxy → teamA ................ WIN                        [PASSA ✗]
  idem, com `ingress policy deny` no backend ...... WIN           [PASSA ✗]
```

Controlo positivo: o mesmo pedido a partir do host devolve `WIN`; o backend responde a si
próprio; `nc` a portas 9 e 53/TCP do gateway dá `rc=1` (recusa) e à 8080 dá `rc=0`
(aceite) — os containers **alcançam** o listener do proxy.

**Causa-raiz** — dois factos que se compõem, nenhum deles um bug isolado:

1. O proxy corre **dentro do netns do pin**. O tráfego proxy→backend **origina-se no
   holder**, logo atravessa `output`/`postrouting`, **nunca o hook `forward`** — e todas as
   chains de política (`fwguard`, `fwdeny`, `fwcont`, `forward`) estão no `forward`. A
   firewall por-container não vê este tráfego, por construção.
2. O holder **não tem chain de `input`** (facto já registado no `AGENTS.md`, ali como razão
   para o publish não precisar de DNAT). Por isso qualquer container alcança o listener do
   proxy no IP do gateway da sua bridge.

O resultado é um relay não governado: quem alcança o gateway alcança **qualquer** backend
registado, em **qualquer** namespace. O FQDN nem precisa de ser adivinhado — é
`<nome>.<namespace>.delonix.internal`, e o DNS interno resolve nomes.

#### Spike GO/NO-GO das duas contenções (medido, sem alterar código)

**Opção A — chain de `input` no netns do pin.** Prototipada **à mão** dentro do netns, com
`nft -f`, e depois removida — sem tocar em código nem respawnar o holder:

```
chain dlxinput { type filter hook input priority filter; policy accept;
                 iifname "tap0" accept ; iifname "lo" accept ; tcp dport 8080 counter drop }
```

| Medição | Antes | Depois |
|---|---|---|
| host → proxy → backend (o propósito do ingress) | `WIN` | `WIN` |
| **teamB → proxy → backend (a fuga)** | `WIN` | **`download timed out`** |
| DNS do FQDN interno a partir de um container | 1 registo A | 1 registo A |
| teamA → teamA directo (não-regressão) | — | `WIN` |
| contador da regra de drop | — | `6 packets / 360 B` |

**GO.** Fecha a fuga, preserva o ingress, e o contador prova que foi a regra. Duas
verificações que interessavam e passaram: a descoberta de serviço **não** depende do proxy
— `s46-be.teamA.delonix.internal` resolve para `10.210.243.29`, o IP do próprio container,
logo container→container por nome vai directo e continua governado pelo isolamento; e o
tráfego legítimo dentro da mesma namespace não foi afectado.

Alcance: é estrutural — cobre **qualquer** listener residente no holder, presente ou
futuro, não só este proxy. Custo por pacote: 3 regras num hook que hoje não tem chain
nenhuma. Limitação: é uma fronteira grossa — um container deixa de poder usar o proxy
**mesmo dentro da sua própria namespace** (por exemplo para terminação TLS, ou para
alcançar um backend de um `kind: HTTPRoute` manual).

**Opção B — o proxy decide ao nível L7.** Viável e mais fina, confirmado por leitura:
`ingress_proxy.rs:319` já é `let (stream, _peer) = listener.accept().await` — **o endereço
do cliente existe e está a ser descartado**; e o processo já resolve `state_root()`
(linha 545), portanto pode mapear IP→container→namespace. Exige decidir o caso «cliente sem
identidade» (o pedido normal vem de fora do nó e não tem namespace — presumivelmente
permitir), invalidação de cache do registo, e só governa **este** proxy. Não é validável por
um contador do kernel: obriga a reconstruir e respawnar o proxy.

**Não são exclusivas.** A é a contenção estrutural, barata e verificável; B é a política
fina por cima. A recomendação é A primeiro (fecha a fuga já, com prova), e B só se aparecer
o caso de uso de um container querer usar o ingress da sua própria namespace.

#### Estado: A implementada e validada ao vivo com o holder respawnado

A chain `dlxinput` entrou no ruleset base (`ingress_table_ruleset`), com escapatória ruidosa
`DELONIX_ALLOW_HOLDER_INGRESS=1`. Confirmada no kernel depois do respawn:

```
chain dlxinput { type filter hook input priority filter; policy accept;
  ct state established,related accept ; iifname "lo" accept ; iifname "tap0" accept
  udp dport { 53, 67, 68 } accept ; tcp dport 53 accept ; meta l4proto icmp accept
  ct state new counter packets 7 bytes 420 drop }
```

| Medição com o código (não com o protótipo) | Resultado |
|---|---|
| host → proxy → backend | `WIN` |
| teamB → proxy → backend | `download timed out` |
| contador da regra de drop | `7 packets / 420 B` |
| DNS interno de dentro de um container | `kaeso-db.default.delonix.internal` → `10.210.58.41` |
| DNS externo (reencaminhamento) | `github.com` resolve |
| saída de um container para a internet | `rc=0` |
| produção durante tudo isto | Odoo `HTTP 303`, Postgres aceita ligação |

O `SERVFAIL` de um nome **simples** (`nslookup kaeso-db`) aparece antes e depois da mudança
— foi medido no início da sessão, com o holder antigo, e não é regressão desta chain.

O teste de regressão afirma a **ordem** (o drop tem de ser a última regra, senão a allowlist
a seguir é regra morta) e foi demonstrado a falhar com a protecção revertida.

---

### 4.3 MÉDIO — o set `@dlxall` só cresce — **CORRIGIDO**

```
@dlxall entries : 49        veths vivos : 8        containers registados : 5
```

`ns_set_join` (`infra.rs:1863`) insere em `@dlxall` e em `@dlxns_<ns>`; **não existe
`ns_set_leave`** e o `do_detach` não remove nada. O comentário da função raciocina
correctamente sobre a mudança de namespace (o join tira o IP do `@dlxns` anterior), mas
nada trata a saída definitiva.

Não é uma fuga de política — `@dlxall` só é lido para *dropar* (`ip saddr @dlxall ct state
new drop`), portanto uma entrada a mais nunca abre nada. É crescimento sem tecto de estado
do kernel e ruído de diagnóstico: um `@dlxall` com 49 endereços num nó com 8 veths não
serve para responder a nenhuma pergunta operacional. Durante esta própria sessão subiu de
**49 para 74** — a fuga medida a acontecer.

**Corrigido** com `ns_set_leave`, numa linha de controlo própria (`nsleave <ip>`) e não
dentro do `detach`: o `detach` não leva endereço nenhum, e o `unfirewall` — que leva — também
é enviado pelo `clear_firewall` para um container **vivo**, pelo que pendurar ali a remoção
despejaria um peer vivo dos sets. Enviada best-effort, por isso um holder antigo limita-se a
recusá-la e comporta-se como sempre, em vez de falhar o teardown.

Validado ao vivo com o holder respawnado: **5 → 4 → 3 → 2** elementos, um por remoção, e os
dois que sobram são exactamente os IPs dos dois containers de produção.

---

### 4.4 MÉDIO — `kind: Pod` ignorava o `spec.network`, e o IP reportado era recomputado

**CORRIGIDO.** Um `kind: Pod` com `spec.network: kaeso-net` arrancou em `10.200.0.2` — a
bridge **default**, não a `kaeso-net` (`10.210.0.0/16`). Não era o manifesto: `create_pod`
passava um **`"ingress"` hardcoded** ao `attach_container` (`pod.rs:148`), e a palavra
`network` não aparecia mais nenhuma vez no módulo. O campo era parseado, documentado como
extensão delonix, e não tinha efeito nenhum — um pod ficava inalcançável da rede que pediu e
alcançável por tudo o que estava na que levou.

**A correcção óbvia sozinha teria posto o motor a mentir**, e é a parte que interessa
registar. `ls`/`describe`/`rm` nunca **liam** o endereço: recomputavam-no com
`infra::container_ip`, que fixa o prefixo default. Estava acidentalmente certo enquanto
todos os pods caíam na bridge default; assim que a rede passa a ser respeitada, os três
passaram a reportar — e o `rm` a **detachar** — um endereço que o pod nunca teve. Medido:
`pod create` dizia `10.210.0.2` e `pod ls`, ao lado, dizia `10.200.0.2`.

O endereço real passa a ser gravado numa label no momento do attach
(`delonix.io/pod-ip`, o mesmo idioma «membership a partir de labels» do `POD_LABEL`, sem
store novo), com recurso à recomputação antiga para pods criados antes da label.

Validado ao vivo: `create`/`ls`/`describe` dizem os três `10.210.0.2`, igual ao que o kernel
tem na netns (`ip netns exec pod-d46-pod` → `10.210.0.2/16`); o isolamento cross-namespace
do pod continua a bloquear na rede custom; e o `rm` limpa a netns sem restos. **A linha 9 da
matriz passa a ✅.**

> **Lição, a mesma de §4.1 por outro caminho:** o valor não estava a ser lido de onde foi
> decidido — estava a ser *re-derivado* por uma fórmula que só coincidia com a realidade
> enquanto houvesse um só caso. Uma fórmula que reproduz o estado em vez de o ler é uma
> segunda fonte de verdade à espera de divergir.

---

### 4.5 BAIXO — `net ingress clear` não avisa que a política sobrevive

`clear` faz exactamente o que promete (*«Remove all inbound rules»*) e `firewall.rs:849` só
descarta a firewall inteira quando **também** as duas políticas estão no default — é
desenho, não defeito, e o `ingress ls` mostra o `deny` que fica. O que falta é uma linha:
depois de `d46-c: removed 0 inbound rule(s)` o container continua totalmente fechado e o
output não o diz. Uma nota a nomear `net ingress policy <c> allow` fecha-o.

*(Registado também porque a caracterização inicial desta observação estava errada — foi
lida como bug antes de o código ser lido, e o código desmentiu-a.)*

---

## 5. Achados — bloco A2 (namespace)

Superfície real, lida do código:

```
$ grep -rln 'pub namespace' crates/*/src/*.rs
crates/delonix-runtime-core/src/lib.rs      (Container)
crates/delonix-vm/src/lib.rs                (Vm)
```

**Só `Container` e `Vm` têm namespace.** `Network`, `Volume`, `Storage`, `ShareVolume`,
`Secret`, `Image`, `HTTPRoute`/`Ingress`, `Dependency` e o projecto `compose` **não têm
campo nenhum** — logo `metadata.namespace` neles é aceite pelo parser e não tem efeito, que
é a forma de falha silenciosa que a regra 2 do ciclo proíbe. (Não confirmado comando a
comando: é a leitura do registo, não uma medição por recurso.)

**`container describe` nunca imprime a namespace** — `vm describe` imprime
(`vm.rs:2032`, `d.sub("Namespace", …)`), o `container describe` não tem uma única
ocorrência. O critério de aceitação de A2 («o `describe` mostra sempre a namespace
efectiva») falha hoje no recurso mais usado dos dois.

### 5.1 O que foi feito, e a decisão de fundo que ficou

**`container describe` passou a imprimir a namespace, sempre** — `default` incluído. Um
campo que desaparece quando tem o valor de omissão obriga quem lê a adivinhar se o recurso
não tem namespace ou se ninguém lha atribuiu.

**O silêncio dos restantes Kinds passou a aviso.** `manifest::load` avisa quando
`metadata.namespace` é escrito num Kind que não o honra, nomeando os que honram. O aviso
é emitido **antes da expansão do `Stack`**, de propósito: um Stack namespaced propaga a
namespace a todos os filhos, e avisar depois disparava uma linha por filho para um campo
que o utilizador nunca escreveu ali. Confirmado ao vivo: um `kind: Network` de topo com
namespace avisa (EN e PT), um Stack namespaced com três filhos avisa **zero** vezes e a
namespace chega aos três.

**Decisão registada — namespace é isolamento de rede, não espaço de nomes.** Só
`Container`/`Pod`/`Vm` a honram porque só um workload com endereço participa nas regras
que a implementam (`@dlxns_<ns>` + o drop cross-namespace). Dar namespace a `Volume`/
`Storage`/`Secret` significaria **escopo de NOMEAÇÃO** — dois namespaces poderem ter um
volume `db` — e isso é uma mudança de chave dos stores com pergunta de migração própria.
É exactamente o item `Storage`/`ShareVolume` do ciclo, que está marcado como decisão a
tomar contigo; não foi antecipado aqui.

**Já satisfeito, verificado no código:** os três Kinds que a honram persistem-na
(`Container.namespace` e `Vm.namespace` com `#[serde(default)]`; o pod deriva-a dos
registos dos membros) e reconstroem-na no `start` e na recuperação pós-respawn.

---

## 6. Estado do host no fim

Banco de ensaio removido por inteiro: 4 containers, 1 pod, 1 rede, 2 rotas órfãs (estas à
mão, ver §4.1) e o proxy parado. Confirmado: `container ls -a` mostra só os 5 de produção
(`kaeso-odoo18-multi`, `kaeso-odoo18`, `kaeso-odoo-8016`, `kaeso-db`, `kaeso-db18`), o
holder não foi tocado e os 5 mantiveram-se a correr durante toda a sessão.

---

## 7. Ordem de trabalho sugerida

1. **§4.1** — persistir `expose` + tornar o `auto_deregister` incondicional. Pequeno,
   testável sem holder, e fecha a 5.ª ocorrência de uma armadilha reincidente.
2. **§4.2** — contenção do proxy L7. É o achado de maior alcance e o único que exige
   decisão de comportamento; não deve entrar à pressa.
3. **§5** — `container describe` a imprimir a namespace (trivial), e depois a decisão de
   fundo sobre que recursos passam a ter namespace de facto.
4. **§4.3 / §4.4 / §4.5** — por esta ordem.

---

## 8. Bloco B1 — volumes sem perda de dados

Medido antes de mudar, com o inventário completo das escritas de metadados por crate.

| Exigência do B1 | Estado medido | Acção |
|---|---|---|
| Escritas de metadados atómicas (temp + `fsync` + modo na criação + rename) | `Store`/`JsonStore`/`delonix-volume`/`ipam.rs` já atómicos; **o `NetworkStore` não** (7 escritas com `fs::write` cru), `delonix-vm` com 2 | ✅ corrigido |
| `fs::remove_dir_all` não apaga contabilidade primeiro | Já correcto: o `remove` do volume apaga «tudo EXCEPTO os metadados» e só depois o `meta.json`, com a razão do EACCES/subuid escrita no código | ✅ nada a fazer |
| Directório ilegível ≠ vazio | Já correcto: `Usage { bytes, unreadable }` e `QuotaState { measured }` — medição incompleta é *desconhecida*, nunca zero | ✅ nada a fazer |
| Volumes de rede: o que acontece a uma escrita em curso quando o NAS desaparece | Lido das opções emitidas: só `credentials=`/`ro`/extras — **sem `soft`/`timeo`/`retrans`**, logo vale o `hard` do NFS | ⚠️ documentado, default mantido |

### 8.1 O `NetworkStore` era o único store do seu próprio crate sem escrita atómica

O `ipam.rs`, no mesmo crate, usa `write_atomic` desde sempre. O `NetworkStore` gravava os
seus sete registos com `fs::write`, que **trunca** o destino e só depois o enche. Dois modos
de falha, e o segundo é o mau:

- um corpo multi-linha rasgado perde linhas, e o `get()` ignora chaves em falta **de
  propósito** (é o que deixa um binário antigo ler um registo novo), por isso a rede volta
  **degradada** em vez de falhar;
- o octeto base é pior: o `get()` parseia um número nu como o formato antigo, logo `"142"`
  truncado a `"14"` continua a ser um octeto **perfeitamente válido**. A rede muda de `/16`
  em silêncio e todos os containers do prefixo antigo ficam sem se alcançar.

Não é hipotético. Com a correcção revertida, o teste novo apanha leitores concorrentes a
receber `network 'ov' is corrupted` — repetidamente, em vários threads.

**Ficam duas `fs::write` cruas, e a razão está no código**: os dois sysctls em `/proc`. O
kernel toma o valor na escrita, não há ficheiro para publicar nem nada para rasgar, e o
`rename` do `write_atomic` nem sequer é aceite ali.

### 8.2 NFS: um NAS que desaparece bloqueia para sempre — e é o comportamento certo

Sem `soft`/`timeo`, uma escrita em curso não falha: **bloqueia indefinidamente** em sono
ininterruptível, e o processo não pode ser morto até o servidor responder. O default fica
como está de propósito — `soft` transforma a mesma falha num `EIO` a meio de uma escrita, o
que para uma base de dados é corrupção silenciosa em vez de uma paragem, e é exactamente o
que este bloco existe para evitar.

Documentado porque o sintoma que o operador vê («o container está preso e não morre») não
aponta para o NAS. A escapatória já existe e não precisa de flag nova: passar
`soft,timeo=50,retrans=2` nas opções extra de montagem.

**NÃO medido** — este host não monta NFS/CIFS de todo (`mount -t` exige `CAP_SYS_ADMIN`,
indisponível em rootless). O acima é lido das opções emitidas e do `nfs(5)`, não observado.
