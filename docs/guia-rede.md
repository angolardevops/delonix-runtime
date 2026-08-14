# Guia de rede do Delonix Runtime

> Do básico ao avançado, para quem opera **só** com o `delonix` — sem PaaS, sem
> control plane, sem daemon. Escrito contra o binário real; cada saída aqui foi
> obtida a correr o comando, e o que não corre neste ambiente está assinalado
> como tal em vez de ser inventado.

---

## 0. O que esta ferramenta é — e o que não é

O `delonix` é o **mecanismo de um host**. Cria bridges, netns, veth, regras nft e
túneis nessa máquina. Não sabe o que é um tenant, não tem base de dados de
utilizadores, não fala com outros nós a não ser quando lhe dizes explicitamente
quais são (`--peer`).

| | `delonix` (este guia) | `delonixctl` |
|---|---|---|
| onde corre | **no** host que vai ser alterado | de qualquer lado |
| como fala | chamadas ao kernel, ficheiros em `$DELONIX_ROOT` | só HTTP, `/v2/*` |
| unidade | container, VM, rede, regra | app, addon, team, org |
| quem autoriza | o utilizador Unix que corre o comando | token Bearer + RBAC |

Se precisas de tenants, quotas, RBAC ou de operar dez máquinas de uma vez, é o
`delonixctl` — este guia não te vai lá levar. Se precisas de segmentar redes numa
máquina e provar que estão segmentadas, é aqui.

### Duas propriedades que explicam quase tudo o resto

**Rootless.** Um processo sem privilégios não tem `CAP_NET_ADMIN` no netns do
host — tem-no apenas dentro do seu próprio user+network namespace. Por isso o
Delonix não cria a bridge «no host»: cria-a dentro de um namespace que ele
próprio detém (o *holder*), e liga-lhe os workloads por veth. É também por isto
que só há **um** comando de rede que precisa de root (`network vlan`), e ele
diz-to à cara.

**Daemonless.** Não há processo residente a reconciliar nada. O holder sobe à
primeira necessidade e cai quando o último workload larga (`refcount 0`). Um
comando que não devolveu erro fez o que disse; não há uma fila onde a intenção
fica a marinar.

### Declarado ≠ realizado

`network create` **declara** uma rede: escreve o registo, escolhe o `/16` e o
nome da bridge. A bridge só nasce no netns do holder no primeiro `attach`. Isto
é deliberado — declarar não deve exigir infraestrutura de pé — mas significa que
`network ls` a mostrar uma rede **não** prova que a bridge existe. Para isso é
`net netns status`.

---

## 1. Primeiros passos

```bash
delonix network create app
delonix network ls
```

```
NAME      DRIVER   BRIDGE         SUBNET
app       bridge   dlxn1f6a832c   10.207.0.0/16
backend   bridge   dlxn79b4a5e1   10.220.0.0/16
```

O nome da bridge (`dlxn` + dispersão do nome) é derivado, não escolhido: o mesmo
nome de rede dá sempre a mesma bridge, em qualquer nó. É o que permite a dois
nós de um overlay concordarem sobre o dispositivo sem o combinarem.

Três vistas da mesma rede, por ordem de verbosidade:

```bash
delonix network inspect app              # compacto
delonix network inspect app -o json      # para automação
delonix network describe app backend     # bloco legível, estilo kubectl describe
```

```
name:     app
driver:   bridge
bridge:   dlxn1f6a832c
subnet:   10.207.0.0/16
gateway:  10.207.0.1
```

Remover:

```bash
delonix network rm app
```

Remove o registo, o uplink VXLAN (se for um overlay) **e** manda apagar a bridge
no holder.

> **Não verifica se há workloads ligados.** Não há um «network is in use» a
> travar-te: a bridge desaparece e os containers que estavam nela ficam sem
> rede, sem serem parados. Se isso for indesejável no teu ambiente, o gate tem
> de ser teu — um `delonix net netns status` antes, ou uma regra no
> procedimento. Vale a pena saber disto antes de escrever um `rm` num script.

---

## 2. O espaço de endereços

Há **dois** regimes, e confundi-los é a origem de metade das surpresas.

### 2.1 Sem `--subnet`: o motor escolhe, e escolhe `10.<200-254>.0.0/16`

```bash
delonix network create app
```

O registo em disco guarda **um octeto**; a bridge, o gateway e a gama de IPAM
são todos derivados dele. É o caminho por omissão e o mais barato de raciocinar:
uma rede, um `/16`, sem sobreposição possível.

### 2.2 Com `--subnet`: qualquer privado, `/8` a `/28`

```bash
delonix network create backend --subnet 10.220.0.0/16
delonix network create pequena --subnet 10.221.0.0/24     # /24 é aceite
delonix network create legado  --subnet 192.168.5.0/24    # 192.168 também
```

Aqui o registo guarda o CIDR inteiro, e as regras são outras: tem de ser espaço
**privado (RFC 1918)** — `10.0.0.0/8`, `172.16.0.0/12` ou `192.168.0.0/16` — com
prefixo entre `/8` e `/28`, e não pode **sobrepor-se** a uma rede já existente.
As duas recusas, verbatim:

```
error invalid argument: subnet '8.8.8.0/24': outside the private address space
(RFC 1918). Use a private range — `10.0.0.0/8`, `172.16.0.0/12` or
`192.168.0.0/16` — with a prefix between /8 and /28, or omit --subnet to let the
engine pick a free one

error conflict: subnet 10.220.0.0/24 overlaps network 'x' (10.220.0.0/24)
```

O `spec.subnet` de um `kind: Network` segue exactamente as mesmas regras — o
declarativo e o imperativo não divergem aqui (verificado a aplicar os três casos
acima por manifesto).

### 2.3 O tecto: 55 redes auto-atribuídas

O alocador automático tem `10.200`–`10.254`, ou seja **55** lugares. À 56.ª rede
criada **sem** `--subnet`:

```
error conflict: no free /16 left for network 'a-mais': the workload space
10.200.0.0-10.254.255.255 holds 55 networks and all are taken — remove one
(`delonix network rm <name>`) to free a subnet
```

Uma recusa, não um duplicado silencioso. O tecto é do alocador, não do motor:
redes com `--subnet` explícito não saem desses 55 lugares, portanto uma
organização que precise de mais do que isso pode continuar com endereçamento
próprio. Ainda assim, vale a pena decidir isto cedo — numa segmentação com uma
rede por equipa e sessenta equipas, a unidade de isolamento passa
provavelmente a ser a *regra de firewall por workload* (secção 5) e não a rede.

---

## 3. Os drivers

| driver | o que faz | rootless |
|---|---|---|
| `bridge` | bridge própria + `/16` próprio, filtrada pela firewall | **sim**, é o caminho realizado |
| `macvlan` | interface do container directamente na LAN física | registada, **não realizada** |
| `ipvlan` | idem, partilhando o MAC do parent | registada, **não realizada** |
| `overlay` | VXLAN entre nós, opcionalmente cifrado com WireGuard | sim, no holder |

`macvlan`/`ipvlan` precisam de `CAP_NET_ADMIN` no netns do host. Sem privilégio,
o `create` regista a rede e **avisa** que não a realizou — não finge que
realizou (é o que o próprio comando documenta; não foi possível confirmar ao
vivo aqui por não haver NIC física disponível para `--parent`). Duas notas que costumam apanhar quem vem do Docker:

- não são filtradas pela firewall do Delonix; o tráfego sai directo para a LAN;
- exigem `--parent` e `--subnet`, e a NIC do `--parent` tem de **existir mesmo** —
  senão a recusa é imediata (`parent NIC 'eth0' does not exist on the host`).

```bash
delonix network create lan --driver macvlan --parent eno1 --subnet 192.168.1.0/24
```

---

## 4. Publicar portos

```bash
delonix net ingress publish web 8080:80      # host 8080 → container 80
delonix net ingress publish web 8443         # mesmo número dos dois lados
delonix net ingress publish dns 5353:53/udp  # UDP
```

**Rootless não liga portos do host abaixo de 1024.** Não é uma limitação do
Delonix — é do kernel. Publica alto e põe um proxy a deter o :443, ou dá a
capability ao binário se a política da máquina o permitir.

Ver e desfazer:

```bash
delonix net ingress ls web
delonix net ingress unpublish web 8080
```

---

## 5. Segmentação — a parte que interessa a quem tem de a provar

### 5.1 O que já está isolado sem fazeres nada

Duas redes `bridge` distintas **não se falam**. Não é uma regra que se escreva; é
a ausência de rota entre os dois segmentos. É o estado inicial e o estado a que
se volta quando se apaga tudo.

### 5.2 Abrir um caminho — e só num sentido

```bash
delonix network route web db          # web PODE iniciar para db
delonix network route web db --rm     # fechar
```

`route` é **direccionado**: abrir `web → db` não abre `db → web`. E, lido com
rigor, um `route` diz que o pacote *pode atravessar*, não que é *permitido* — a
firewall por workload continua a decidir a seguir. São duas camadas, de
propósito: a rota é topologia, a regra é política.

> Precisa do holder de pé. Se ainda não há nada ligado, faz `delonix net netns
> up` primeiro.

### 5.3 Firewall por workload: entrada

O padrão para um serviço interno é *default-deny* e depois nomear o que entra:

```bash
delonix net ingress policy db deny
delonix net ingress allow db tcp/5432 --from 10.200.0.0/16
delonix net ingress ls db
```

A regra é avaliada **depois** do DNAT, portanto o porto que se nomeia é o que o
container escuta (5432), não o publicado no host. Enganar-se aqui é a origem
mais comum de «abri a regra e continua a não entrar».

Formas aceites em `<PORT>`: `tcp/5432`, `udp/53`, `5432` (qualquer protocolo),
`tcp/*` (todos os portos).

`--note` guarda um texto com a regra. Numa auditoria daqui a um ano, é a
diferença entre uma regra justificada e uma regra que ninguém se atreve a
apagar.

### 5.4 Firewall por workload: saída

```bash
delonix net egress policy app deny
delonix net egress allow app udp/53                        # senão nada resolve
delonix net egress allow app tcp/5432 --to 10.200.0.20/32
delonix net egress allow app tcp/443
```

O `udp/53` não é decoração: um *default-deny* de saída tira o DNS, e o sintoma
que chega ao operador é «a aplicação está lenta», não «a firewall bloqueou».

**Uma mudança de política não derruba fluxos já estabelecidos** — só decide os
novos. Para verificar uma alteração, reinicia o workload ou espera que a ligação
caia.

### 5.5 Saída ao nível da rede

Além do por-container, uma rede inteira tem política de saída:

```bash
delonix net egress net app deny                              # sem Internet
delonix net egress net app allowlist --to 10.0.0.0/8,1.1.1.1/32
delonix net egress show app
```

E, o que CIDRs não conseguem exprimir, uma lista de nomes:

```bash
delonix net egress host app github.com          # apanha github.com e *.github.com
delonix net egress host app registry.npmjs.org
```

Os endereços são aprendidos à medida que as respostas de DNS passam, portanto um
CDN que renumera continua a funcionar — que é exactamente o caso em que uma
lista de CIDRs escrita à mão apodrece em silêncio.

### 5.6 Um desenho de referência

Três zonas, entrada só onde é preciso, saída fechada por omissão:

```bash
# zonas
delonix network create dmz
delonix network create app
delonix network create dados

# caminhos, sempre num sentido
delonix network route dmz app
delonix network route app dados

# entrada
delonix net ingress policy api deny
delonix net ingress allow api tcp/8080 --from 10.201.0.0/16   # só da dmz
delonix net ingress policy pg deny
delonix net ingress allow pg tcp/5432 --from 10.202.0.0/16    # só da app

# saída
delonix net egress net dados deny                              # a base não sai
delonix net egress policy api deny
delonix net egress allow api udp/53
delonix net egress allow api tcp/5432 --to 10.203.0.0/16
```

Repara que a `dados` não tem rota **de** lado nenhum a não ser da `app`, e não
tem saída nenhuma. Se um dia alguém precisar de a exportar, terá de escrever um
comando — e esse comando fica no histórico.

---

## 6. Multi-nó: overlay VXLAN cifrado

Cada nó tem uma identidade WireGuard própria. A chave privada fica `0600` em
`<root>/wg/node.key`; a pública é o que se distribui.

```bash
delonix network node init     # idempotente; imprime a chave pública
delonix network node key      # só a pública, para compor num script
```

Em cada nó, a mesma rede com o mesmo VNI:

```bash
# nó A (10.0.0.1)
delonix network create mesh --driver overlay --vni 42 \
  --peer 10.0.0.2=<pub_B>=10.42.0.2 --wg-ip 10.42.0.1

# nó B (10.0.0.2)
delonix network create mesh --driver overlay --vni 42 \
  --peer 10.0.0.1=<pub_A>=10.42.0.1 --wg-ip 10.42.0.2
```

`--peer` aceita `<ip>` (VXLAN em claro) ou `<ip>=<pubkey>=<wg_ip>` (cifrado), e é
repetível.

Duas coisas a saber antes de pôr isto em produção:

- **cifrado exige `wg` no host.** Se declaras `--wg-ip` e o `wireguard-tools`/
  módulo não estão lá, o comando **falha antes** de levantar o VXLAN. É
  deliberado: sem isso a FDB apontaria para endereços só alcançáveis pelo túnel
  que nunca subiria, e terias um overlay a fingir que está de pé.
- **MTU.** VXLAN come 50 bytes, WireGuard mais 60. Um overlay cifrado sobre um
  MTU de 1500 deixa ~1390 úteis. Se as aplicações tiverem trocas grandes e
  `DF`, isto aparece como lentidão inexplicável e não como erro.

---

## 7. Declarativo

Para rede versionada em git, em vez de uma sequência de comandos:

```bash
delonix network apply -f delonix-manifest.yaml
```

Aplica **só** os documentos `kind: Network` do manifesto, deixando os outros
Kinds em paz, e é idempotente por nome. Para o manifesto inteiro (todos os
Kinds, por ordem de dependência) é `delonix stack apply`.

O esquema sai do próprio binário, não de um `.md` que envelhece:

```bash
delonix explain Network       # referência de campos, estilo kubectl explain
delonix schema --help         # JSON Schema gerado do código (pede um alvo)
```

O manifesto precisa de `apiVersion: delonix.io/v1` — sem ele a recusa é
`missing field \`apiVersion\``, que é clara mas apanha quem colou um exemplo
truncado:

```yaml
apiVersion: delonix.io/v1
kind: Network
metadata:
  name: dados
spec:
  subnet: 10.221.0.0/24
```

---

## 8. Observar

```bash
delonix net netns status          # a PRIMEIRA coisa a ler quando não há rede
delonix net netns status --json
```

```
ingress DOWN — pin — · control in-pin · slirp — · bridge delonix0 (10.200.0.1) · refcount 0
```

Lê-se: infra em baixo, sem slirp, `refcount 0` (nenhum workload a segurá-la). Com
`refcount > 0` e `ingress UP`, a bridge existe mesmo.

```bash
delonix net flow             # RX/TX por container, agora
delonix net flow -w          # redesenhado de 2 em 2s
delonix network dash         # TUI de redes
delonix network dash --once  # um instantâneo, para pipe
delonix network dash --json
```

O `net flow` usa eBPF quando está disponível e **degrada para contadores de
veth** quando não está — dizendo qual dos dois está a usar, em vez de dar
números sem procedência.

---

## 9. O único comando que precisa de root

```bash
delonix network vlan eth0 100              # DRY-RUN, não muda nada
sudo delonix network vlan eth0 100 --apply
sudo delonix network vlan eth0 100 --rm --apply
```

Uma interface 802.1Q numa NIC do host precisa de `CAP_NET_ADMIN` no netns do
host, que nenhum utilizador sem privilégios tem. Por isso este comando é
dry-run por omissão: imprime o plano e só toca na máquina com `--apply`.

---

## 10. Diagnóstico

### «Criei a rede e o container não tem rede»

Ordem de leitura:

1. `delonix net netns status` — a infra está de pé? `refcount` faz sentido?
2. `delonix network inspect <rede>` — a bridge e o `/16` são os que esperas?
3. `delonix net ingress ls <container>` / `net egress ls <container>` — há uma
   política *deny* sem a regra correspondente?
4. `delonix net flow` — passa alguma coisa?

### «Outro `DELONIX_ROOT` já detém a infra»

```
error system call `control socket` failed: another delonix state root on this
user already owns the network infra: `/tmp/delonix-net-1000/control.sock` has a
live listener, but there is no pidfile under `<...>/ingress`. The sockets are
per-USER while the pidfiles are per-ROOT, so rebuilding from here would delete
that infra and unplug every workload on it. Either use that root (unset/point
`DELONIX_ROOT` at it), or stop it deliberately with `delonix net netns down`
from the root that owns it.
```

Os sockets de controlo são por **utilizador**; os pidfiles são por **root de
estado**. Correr dois `DELONIX_ROOT` diferentes com o mesmo uid é a receita para
isto. A recusa é intencional: reconstruir dali abaixo desligaria os workloads da
outra infra.

### «A regra está lá e não passa»

- O porto na regra de *ingress* é o do **container**, não o do host.
- Uma mudança de política não corta ligações já estabelecidas.
- `macvlan`/`ipvlan` **não passam pela firewall** do Delonix.

---

## 11. Referência

### `delonix network` — redes de utilizador

| comando | o que faz |
|---|---|
| `create <nome>` | cria; `--driver`, `--subnet`, `--parent`, `--gateway`, `--vni`, `--peer`, `--wg-ip` |
| `ls` | lista; `-o table\|json` |
| `inspect <nome>` | detalhe; `-o table\|json` |
| `describe <nomes>...` | bloco legível, várias redes |
| `rm <nome>` | remove registo + bridge |
| `route <de> <para>` | caminho **direccionado**; `--rm` fecha |
| `vlan <nic> <id>` | 802.1Q; **root**, dry-run sem `--apply`; `--rm` |
| `apply -f <ficheiro>` | aplica os `kind: Network` de um manifesto |
| `dash` | TUI; `--once`, `--json` |
| `node init\|key` | identidade WireGuard deste nó |

### `delonix net` — plumbing

| grupo | comandos |
|---|---|
| `netns` | `up`, `down`, `status`, `attach`, `detach`, `exec`, `publish`, `unpublish`, `firewall` |
| `ingress` | `ls`, `allow`, `deny`, `policy`, `publish`, `unpublish`, `clear`, `rm` |
| `egress` | `ls`, `show`, `allow`, `deny`, `host`, `net`, `policy`, `clear`, `rm` |
| `flow` | tráfego por container (`--iface`, `-w`) |
| `httproute` | `apply`, `ls`, `rm` — proxy L7 embebido |
| `tunnel` | `expose`, `ls`, `describe`, `apply`, `rm` — exposição pública |
| `boot` | `enable`, `disable`, `status` — unidades systemd pós-reboot |

Global: `--l18n en|pt` (ou `$DELONIX_L18N`) muda a língua da saída, antes de
qualquer subcomando.

---

## 12. Limites conhecidos

Escritos aqui para não serem descobertos em produção:

- **55 redes** `bridge` por host (`10.200`–`10.254`), com recusa explícita ao
  passar do tecto.
- **`macvlan`/`ipvlan` não são realizados sem privilégio** — ficam registados,
  com aviso.
- **Portos do host < 1024 não se publicam** em rootless.
- **`route` e as políticas de egress ao nível da rede precisam do holder de pé.**
- **`network rm` não verifica se há workloads ligados** — apaga a bridge à mesma.
- **Sem IPv6** nas redes bridge.
- **Só espaço privado** em `--subnet` (RFC 1918, `/8`–`/28`).
- **Um `DELONIX_ROOT` por utilizador** de cada vez, para a infra de rede.
- **O `ls` mostra o declarado.** Para saber o que está realizado, `net netns
  status`.

---

## 13. Onde continuar

- `delonix explain Network` — os campos do `kind: Network`, a partir do código.
- `docs/comandos/` — referência gerada, um ficheiro por comando.
- `docs/adr/` — as decisões e o porquê (a rede é sobretudo a ADR-0013).
- `docs/cli-stability.md` — o que pode partir num upgrade e o que não pode.
