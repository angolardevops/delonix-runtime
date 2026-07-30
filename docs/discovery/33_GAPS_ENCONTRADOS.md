# 33 — Discovery: endurecimento do datapath de Ingress e Egress

| Campo | Valor |
|---|---|
| Data | 2026-07-28 |
| Linha de base do prompt | `0.35.1` (commit `60efdd6a7`) |
| **Linha de base real no `main`** | **`0.37.0`** (commit `55db34d`) — duas versões à frente do prompt |
| Método | Leitura de código + testes exploratórios ao vivo num host rootless real |
| Alterações de produção | Nenhuma. Este documento é o único artefacto. |

> **Aviso de leitura.** Tudo o que segue foi medido ou lido. Onde não foi possível
> medir, está escrito `INDETERMINADO` e diz-se porquê. Nenhuma classificação é
> inferida da documentação.

## Estado da execução

| Bloco | Requisitos | Estado |
|---|---|---|
| **0 — Segurança imediata** | RF-NET-11 (mitigação), RF-NET-02 | ✅ **FEITO** — ver `docs/releases/v0.37.1.md` |
| 1 — A′ | RF-NET-03, RF-NET-05 (reformulado) | por fazer |
| 2 — A″ | RF-NET-01 (reformulado) | por fazer |
| 3 — B | RF-NET-07 → RF-NET-06 | por fazer |
| 4 — C | RF-NET-12, 13, 14(a) | por fazer |
| 5 — D | RF-NET-04 | por fazer |
| 6 — E | RF-NET-11 (paridade completa), 10, 09(reduzido) | por fazer |
| — | RF-NET-08, RF-NET-14(b) | fora, à espera de ADR |

---

## 0. Passo 0 — delta de versões

### 0.1 A linha de base do prompt já está desactualizada

O prompt declara `0.35.1` e a fonte da especificação `0.32.2`. O `main` está em
**`0.37.0`**. Entre `v0.32.2` e HEAD há **6 commits** a tocar `crates/delonix-net/`,
dois deles substanciais:

| Commit | O que fez |
|---|---|
| `3c0d1a4` | 5 bugs do flow `-p` ↔ firewall, **2 de segurança**: porta descartada em silêncio quando `proto: any`; multi-homing contornava firewall **e** isolamento de namespace |
| `f8ab32b` | Endurecimento L4: `ct state` na política default, aplicação atómica, verdict map, `counter` em todas as regras, recusa de IPv6 na validação, ranges de portas |

`f8ab32b` foi produzido numa sessão de trabalho imediatamente anterior a este
discovery e **fecha ou altera parcialmente 4 dos 14 requisitos** — ver a coluna
"Evidência" de RF-NET-05, 09, 11 e 13.

### 0.2 O desvio de versão anunciada — confirmado

- `README.rst:18` → `:Version: 0.32.2`
- `Cargo.toml` → `version = "0.37.0"`

Cinco versões menores de desvio. O item 10 dos deliverables (verificação em CI) é,
portanto, **justificado e confirmado**.

### 0.3 Divergência `--help` vs cheatsheet

`delonix net --help` na `0.37.0` expõe `netns`, `flow`, `ingress`, `egress`,
`httproute`, `tunnel`, `boot`. A reorganização que criou o grupo `net` (v0.30.0) foi
**breaking sem aliases**, pelo que qualquer cheatsheet anterior a essa versão
documenta caminhos de CLI que já não existem (`delonix ingress …` em vez de
`delonix net ingress …`). O cheatsheet da `0.32.2` é posterior a essa mudança, logo
o desvio principal é de conteúdo (comandos novos ausentes), não de caminho.

### 0.4 Nota de processo — trabalho arrastado para um commit alheio

O trabalho de `f8ab32b` foi escrito nesta árvore sem ser commitado, e foi **apanhado
e commitado por uma sessão paralela** que estava a fechar a `v0.37.0`. Já está em
`origin/main`. O conteúdo está correcto e testado, mas não foi revisto por quem o
commitou. É a armadilha conhecida do worktree partilhado deste repositório e vale a
pena confirmar o `f8ab32b` numa revisão dedicada.

---

## 1. Inventário

### 1.1 Mapa das cadeias nftables

Uma única tabela, no netns **efémero** do holder:

```
table ip dlxing
├── set  dlxall              todos os IPs de container da SDN
├── set  dlxns<hash>         um por namespace lógico
├── set  dlxfq<hash>         um por bridge — FQDNs aprendidos
├── map  fwmap               ipv4_addr : verdict   (dispatch por container)
├── chain pre      nat    prerouting  -100   DNAT das portas publicadas
├── chain post     nat    postrouting  100   masquerade oifname tap0
├── chain fwdeny   filter forward      -10   egress global/por-rede, l4guard
├── chain fwcont   filter forward       -5   ip {daddr,saddr} vmap @fwmap
├── chain forward  filter forward        0   policy drop + established/related
└── chain fw<hash>                            uma por container (alvo do vmap)
```

Confirmado ao vivo: `nft list tables` dentro do holder devolve **exactamente**
`table ip dlxing`. Nada mais.

**Excepção encontrada**: `crates/delonix-runtime-bin/src/cmd/vmbridge.rs:116-119,161-164`
escreve `iptables -I FORWARD` na tabela **do host**. É o comando `vm bridge`
(privilegiado, opt-in, root). Viola a restrição 5 da secção 5 do prompt.

### 1.2 Ponto de aplicação da política no ciclo de vida

`cmd_start` (`crates/delonix-runtime-bin/src/cmd/container.rs`):

| Linha | Instrução |
|---|---|
| ~3196 | `infra::attach_container(&c.id, &n, &c.namespace)?` — cria a netns, o veth, junta aos sets de namespace |
| ~3264 | `apply_firewall_everywhere(&c, fw)` — programa a chain por container |
| ~3316 | `RunSpec { … }` — só depois é construído o spec do processo |

**A ordem está correcta**: as regras são programadas antes de o processo existir. Não
há janela de tráfego não filtrado no arranque. O que falha é o tratamento de erro —
ver RF-NET-03.

### 1.3 Caminho de aprendizagem DNS

`handle_dns_query` (`infra.rs:~3690`) → `forward_dns(q)` → **`snoop_fqdn(&name, &resp)`**
(`infra.rs:3701`). A aprendizagem ocorre **exclusivamente** sobre respostas que o
próprio resolvedor do holder encaminhou. Não há sniffing promíscuo.

### 1.4 Precedência — medido, não presumido

Três cenários, container real, `nginx:alpine` numa rede custom:

| Cenário | Resultado medido |
|---|---|
| `allow tcp/80` → `deny tcp/80` (mesmo match) | **DENY** — o último comando substitui |
| `deny tcp/80` → `allow tcp/80` (mesmo match) | **ALLOW** — idem, com nota `replaces the previous deny rule` |
| `deny 80` → `allow tcp/80` (matches distintos, sobrepostos) | **DENY** — ganha a **ordem de inserção**, com aviso de sombra que nomeia a regra e o comando de remoção |

Semântica efectiva: **substituição para match idêntico (ufw), ordem de inserção para
matches sobrepostos**. Não existe campo `priority` em lado nenhum.

### 1.5 IPv6 — medido

| Medição | Resultado |
|---|---|
| Tabelas nft no holder | `table ip dlxing` — **só v4** |
| `slirp4netns` argv | sem `--enable-ipv6` |
| Saída v6 para a Internet | `Network is unreachable` |
| Endereços v6 do container | **4** — link-local **e ULA global** |
| Prefixo ULA | `fd00:<2º octeto>::/64` por rede, **atribuído deliberadamente** (`infra.rs:1184-1200`) |
| Endereço ULA do container | `fd00:<o2>::<o3>:<o4>` — **derivável do IPv4** |

Ver RF-NET-11 — é o achado mais grave deste discovery.

### 1.6 Modelo de dados das políticas

Não há store de políticas. A política vive **no próprio container**:
`Container.firewall: Option<ContainerFw>` (`delonix-runtime-core/src/lib.rs:92`), com
`enabled`, `policy_in`, `policy_out`, `rules: Vec<FwRule>`, `namespace`.
`FwRule { dir, proto, port, src, action, note }`.

O manifesto `FirewallPolicy` (`FwDocSpec`, `cmd/firewall.rs:913`) tem `target: String`
— um nome — e **não tem** `selector` nem `priority`. Compila para `ContainerFw` do
container nomeado.

`Container.labels: BTreeMap<String,String>` **já existe** (`runtime-core:375`), pelo
que os dados para RF-NET-06 estão disponíveis; falta o mecanismo.

### 1.7 Métricas

`delonix-runtime-core/src/metrics.rs`: registo Prometheus partilhado, `Counter` e
`Gauge` simples. **Zero `Family<…>`** — não há hoje nenhuma métrica com labels, que é
precisamente a forma que RF-NET-13 exige (`{container, direction, rule_id}`).

### 1.8 `system events`

`delonix-runtime-core/src/events.rs`: `events.jsonl` append-only, `Event { ts, kind,
action, id, name, … }`. O desenho é **deliberadamente sem lock**, apoiado na
atomicidade de um `write` `O_APPEND` abaixo de `PIPE_BUF` (4 KiB) — está documentado
no cabeçalho do módulo. Rotação oportunista para `.1`, uma só geração.

`grep events:: cmd/firewall.rs` → **vazio**: alterações de política de rede **não
emitem eventos hoje**.

---

## 2. Classificação dos requisitos

### RF-NET-01 — Aprendizagem de FQDN não falsificável

- **Estado: PARCIAL** (premissa central refutada, três sub-requisitos confirmados)
- **Evidência:**
  - *Refuta* o cenário do prompt: `infra.rs:3701` — `snoop_fqdn` é chamado **só** a
    partir de `handle_dns_query`, sobre a resposta que o resolvedor do holder acabou
    de encaminhar. Um container que corra o seu próprio resolvedor, ou que forje uma
    resposta, **nunca alimenta o set**. Não há observação promíscua de tráfego.
  - *Confirma* o sub-requisito 1: `infra.rs:2580-2581` — `egress_specs` emite
    incondicionalmente `udp dport 53 accept` e `tcp dport 53 accept` para **qualquer
    destino** sempre que há allowlist. Não existe DNAT da porta 53 para o resolvedor
    interno. Consequências: (a) o resolvedor interno é contornável, (b) sob a política
    de egress mais restritiva que o motor sabe exprimir, resta um **canal de
    exfiltração por DNS tunnelling** para um resolvedor hostil.
  - *Confirma* o sub-requisito 4: `infra.rs:~1943` — `timeout 1h` **hardcoded**. O TTL
    da resposta é ignorado por completo. Não há `--ttl-floor` nem `--ttl-ceiling`.
  - *Confirma* o sub-requisito 5: só o endereço entra no set. Não se regista FQDN,
    instante, expiração nem container de origem; `egress show` lista IPs
    (`egress_set_members`).
  - **Lacuna não prevista**: `snoop_fqdn(name, resp)` **não recebe o cliente**. Casa o
    nome contra os sufixos de **todas** as redes registadas e injecta nos sets
    correspondentes. Um container da rede A pode fazer com que entradas sejam
    injectadas no set da rede B.
- **Impacto real:** menor do que o prompt assume no vector de falsificação, maior no
  de exfiltração. A allowlist de egress não contém dados: o DNS sai sempre.
- **Esforço:** M
- **Dependências:** nenhuma
- **Recomendação: REFORMULAR** — reescrever o requisito em torno de (1) DNAT do :53,
  (4) tecto/piso de TTL, (5) metadados da entrada, e (6) scoping por cliente. O ponto
  2 do requisito original já é verdade hoje.

### RF-NET-02 — Negação por omissão de destinos sensíveis

- **Estado: CONFIRMADO**
- **Evidência:** `grep -rn "169.254|link-local"` sobre `crates/` → **zero ocorrências**
  fora do código de VMs. Não existe qualquer negação por omissão de link-local, de
  metadados, do loopback do host ou dos sockets de gestão.
- **Impacto real:** alto e **agravado** pelo RF-NET-11: `fe80::/10` não é apenas
  "não negado" — é o caminho por onde toda a política é contornável hoje.
- **Esforço:** S
- **Dependências:** deve ser feito **em conjunto** com RF-NET-11, senão a negação
  v4 é decorativa.
- **Recomendação: IMPLEMENTAR**

### RF-NET-03 — Aplicação fail-closed no arranque

- **Estado: PARCIAL** (ordem correcta, tratamento de erro incorrecto)
- **Evidência:**
  - *Ordem, refutada como problema*: `container.rs:~3196` (attach) e `~3264`
    (`apply_firewall_everywhere`) correm **antes** da construção do `RunSpec` em
    `~3316`. Não há janela.
  - *Falha silenciosa, confirmada*: `container.rs:3264-3272` — em erro,
    `eprintln!("warning: firewall/isolation of '{name}' not reapplied on start: {e}")`
    e **o container arranca à mesma**, sem política. O mesmo padrão em `~4300` e
    `~4373` (caminhos de `update`). É literalmente o anti-padrão nº 3 da secção 5.
- **Impacto real:** alto. Um falhanço transitório do holder produz um container a
  correr sem isolamento, com um aviso em `stderr` que ninguém lê num pipeline.
- **Esforço:** S para o `run`/`start`; M para a atomicidade do `update` (o prompt
  exige "a transição é atómica ou reverte", e hoje não há rollback).
- **Dependências:** nenhuma
- **Recomendação: IMPLEMENTAR**

### RF-NET-04 — Reconciliação e detecção de deriva

- **Estado: PARCIAL**
- **Evidência:**
  - *Sub-requisito 1, já satisfeito com ressalvas*: uma tabela própria, confirmada ao
    vivo (`nft list tables` → só `table ip dlxing`). Ressalvas: a família é `ip` e não
    `inet` (RF-NET-11), e `vmbridge.rs:116` escreve na `FORWARD` do host.
  - *Sub-requisitos 2, 3, 5*: `policy verify` não existe; não há qualquer comparação
    entre declarado e efectivo; nenhuma métrica de deriva.
  - *Sub-requisito 4*: `net boot enable` **existe** e instala unidades systemd — o
    ponto de extensão para o temporizador está disponível.
  - **Lacuna não prevista, maior que a do prompt**: o estado nft vive no netns
    **efémero do holder**. Não é preciso um `nft flush ruleset` para o perder — basta
    o holder morrer. O `CLAUDE.md` já regista que o isolamento de namespace **não é
    reconstruído num respawn do holder**. A deriva por reinício do holder é o caso
    comum; a do prompt (agente externo a escrever na tabela) é o caso raro, porque a
    tabela vive num netns que mais ninguém vê.
- **Impacto real:** alto, mas por uma razão diferente da que o prompt aponta.
- **Esforço:** L
- **Dependências:** RF-NET-13 (métricas) para instrumentar; RF-NET-14 (eventos).
- **Recomendação: IMPLEMENTAR**, reformulando o alvo: reconciliação **contra o
  respawn do holder** primeiro, contra escrita externa depois.

### RF-NET-05 — Precedência determinística

- **Estado: CONFIRMADO** (não há `priority`) — **mas a ordem proposta é breaking**
- **Evidência:** secção 1.4 acima, três cenários medidos ao vivo. A semântica actual é
  substituição por match idêntico + ordem de inserção para sobreposições, com aviso de
  sombra explícito. Nenhum campo `priority` existe no `FwRule` nem no `FwDocSpec`.
- **Impacto real:** a ausência de `priority` é uma limitação real de expressividade.
  Mas adoptar "todos os `deny` antes de todos os `allow`" **inverte silenciosamente**
  políticas já em produção: hoje um `allow` inserido antes de um `deny` sobreposto
  ganha; sob a proposta passaria a perder. É exactamente o anti-padrão nº 10.
- **Esforço:** M
- **Dependências:** deve preceder RF-NET-06 e 07 (que multiplicam as regras por
  container e tornam a ordem crítica).
- **Recomendação: REFORMULAR** — acrescentar `priority` como campo **opcional**, com
  a ordem de inserção preservada como critério de desempate e como comportamento por
  omissão. Assim ganha-se determinismo explícito sem quebrar nada. Se o utilizador
  insistir na reordenação global, é `breaking` e exige guia de migração e uma major.

### RF-NET-06 — Selecção por label

- **Estado: CONFIRMADO** (ausente)
- **Evidência:** `FwDocSpec.target: String` (`cmd/firewall.rs:925`), sem `selector`.
  `Container.labels: BTreeMap<String,String>` já existe (`runtime-core:375`), e a CLI
  já tem `container run --label` — os dados estão lá.
- **Impacto real:** alto para multi-tenancy. Sem isto, cada container criado obriga a
  reaplicar política à mão, e o requisito "um container criado depois da política
  herda-a" é impossível de exprimir.
- **Esforço:** L
- **Dependências:** **RF-NET-07** (a compilação para sets nomeados é o mecanismo que
  torna o selector viável sem reprogramar regras a cada evento) e RF-NET-05.
- **Recomendação: IMPLEMENTAR**, depois de 07.

### RF-NET-07 — Origem e destino por identidade

- **Estado: CONFIRMADO** (ausente)
- **Evidência:** `FwRule.src: String` validado por `fw_src_ok` — **só CIDR IPv4**. As
  regras são geradas com **endereços literais** (`fw_rule_tail`, `infra.rs:~2148`),
  que é o anti-padrão nº 4 do prompt descrito como estado actual.
  *A favor da viabilidade*: o motor **já usa sets nomeados** em três sítios
  (`@dlxall`, `@dlxns<hash>`, `@dlxfq<hash>`) e um verdict map (`@fwmap`). O padrão
  está provado nesta base de código; falta generalizá-lo.
- **Impacto real:** alto. É o requisito que desbloqueia 06 e o que torna a política
  exprimível em termos de tenant.
- **Esforço:** L
- **Dependências:** RF-NET-05
- **Recomendação: IMPLEMENTAR** — é a peça central do bloco B e deve vir antes de 06.

### RF-NET-08 — Endereço de saída estável por rede

- **Estado: CONFIRMADO** (ausente) — **com dúvida séria de viabilidade em rootless**
- **Evidência:** um único `slirp4netns` partilhado; `chain post` faz
  `oifname "tap0" masquerade` sem distinção de rede. `network create` não tem
  `--egress-ip`.
- **Impacto real:** real como requisito comercial, mas em **rootless o motor não tem
  acesso às tabelas de encaminhamento do host** — o requisito 2 do prompt (policy
  routing com tabela dedicada) não é satisfazível sem privilégio. As saídas viáveis
  são: um `slirp4netns` **por rede** com `--outbound-addr` (a flag existe, é marcada
  `experimental` pelo upstream), ou modo privilegiado explícito.
- **Esforço:** L (por-rede via slirp) a XL (policy routing privilegiado)
- **Dependências:** nenhuma
- **Recomendação: REFORMULAR** — decidir primeiro, em ADR, entre "um slirp por rede"
  e "modo privilegiado", e escrever o requisito contra a decisão. Como está, o
  requisito 2 é irrealizável sob a restrição 2 da secção 5.

### RF-NET-09 — Endereço real do cliente

- **Estado: PARCIAL** — a premissa principal está **REFUTADA por medição**
- **Evidência:** medido nesta árvore com três clientes contra um `nginx` publicado,
  lendo o log de acessos do próprio container:

  | Cliente | Origem vista pelo container |
  |---|---|
  | `127.0.0.1` | `10.0.2.2` (gateway do slirp) |
  | `172.16.31.103` (LAN) | **`172.16.31.103`** |
  | `192.168.122.1` (gateway libvirt) | **`192.168.122.1`** |

  A libslirp só substitui a origem quando **não tem rota de volta** para ela, que é o
  caso do loopback. Toda a origem roteável passa intacta. Documentado em
  `delonix_net::SLIRP_GW` desde `f8ab32b`.
  *Corolário verificado ao vivo*: `net ingress allow <c> <porta> --from <cidr>`
  **funciona** contra um cliente remoto real (origem permitida 200, outra origem 000).
  Logo os sub-requisitos 2 (PROXY protocol L4) e 3 (`net flow` pós-DNAT) **não têm
  problema a resolver**.
  *Confirma* o sub-requisito 1: `grep -i x-forwarded cmd/ingress_proxy.rs` → **vazio**.
  O proxy L7 não propaga `X-Forwarded-For`/`-Proto`/`-Host`.
- **Impacto real:** baixo, e circunscrito ao proxy L7.
- **Esforço:** S
- **Dependências:** nenhuma
- **Recomendação: REFORMULAR** — reduzir ao sub-requisito 1 (cabeçalhos
  `X-Forwarded-*` + `--trusted-proxies` no `httproute`). Descartar 2 e 3.

### RF-NET-10 — Limites no ingress

- **Estado: PARCIAL**
- **Evidência:** `do_l4guard` (`infra.rs:~1998`) já emite
  `meter dlx_conn_rate { ip saddr limit rate over N/second burst … } counter drop` e
  `meter dlx_conn_count { ip saddr ct count over N } counter drop`, com pré-voo
  `nft -c` e degradação limpa se o kernel não suportar `meter`. Limitações: é
  **global** ao ingress (não por rede, não por regra, não por porta), e só alcançável
  por `kind: FirewallPolicy` com `rateLimit` — `grep set_l4_guard` fora do crate mostra
  **zero comandos de CLI**.
- **Impacto real:** médio. O mecanismo existe e funciona; falta granularidade e
  superfície.
- **Esforço:** M
- **Dependências:** RF-NET-07 (para `ct count` por identidade e não por endereço)
- **Recomendação: IMPLEMENTAR**, reaproveitando `do_l4guard` em vez de o reescrever.

### RF-NET-11 — Paridade IPv4/IPv6

- **Estado: CONFIRMADO — CRÍTICO. Bypass reproduzido ao vivo.**
- **Evidência (reprodução completa, dois containers na mesma rede):**

  ```
  # firewall a NEGAR em IPv4
  $ delonix container exec cli wget -T3 -O/dev/null http://10.216.133.231/
  wget: download timed out                                   → V4_DENY

  # o mesmo destino, o mesmo porto, por IPv6
  $ delonix container exec cli wget -T3 -O/dev/null 'http://[fd00:216::5081:c3ff:fe63:8bd1]:80/'
                                                             → ULA_BYPASS
  ```

  Cadeia de factos que o produz:
  1. `infra.rs:1184-1200` — a SDN **atribui deliberadamente** IPv6 ULA por rede
     (`fd00:<2º octeto>::/64`), com endereço por container **derivável do IPv4**
     (`fd00:<o2>::<o3>:<o4>`). Não é um acidente do kernel: é código.
  2. A firewall inteira é `table ip` — **v4 apenas**. Confirmado: `nft list tables`
     no holder devolve só `table ip dlxing`.
  3. O `nginx:alpine` escuta em `:::80` por omissão (o próprio entrypoint da imagem
     activa `listen [::]:80`), como a esmagadora maioria das imagens modernas.
  4. Descoberta é trivial: um `ping -6 ff02::1%eth0` enumera **todos** os vizinhos da
     bridge numa única passagem (medido: 3 respostas, incluindo o alvo).
- **Impacto real:** **todo o modelo de política é contornável**, não apenas as regras
  explícitas: `policy deny`, isolamento de namespace, `kind: Dependency` e os limites
  do RF-NET-10 são todos `table ip`. Um tenant alcança containers de outro tenant por
  ULA, sem privilégio, com um comando. O `--from` do RF-NET-07 herdaria o mesmo furo.
- **Esforço:** M para a recusa explícita (desligar o ULA e dropar v6 na bridge);
  L para paridade real (migrar para `table inet`, IPAM v6, sets duplos).
- **Dependências:** nenhuma. **Bloqueia a utilidade de 02, 05, 06, 07 e 10.**
- **Recomendação: IMPLEMENTAR PRIMEIRO** — sobe de P1 para P0, à frente de tudo o
  resto. Recomenda-se a mitigação curta (desactivar o ULA + `ip6tables`/regra v6 de
  drop na bridge, ou `table inet` com drop total de v6) numa versão de segurança
  imediata, e a paridade completa como trabalho subsequente.

### RF-NET-12 — `delonix net policy explain`

- **Estado: CONFIRMADO** (ausente)
- **Evidência:** não existe grupo `net policy`. O mais próximo é o veredicto por
  porta publicada (`published_reach`, `cmd/firewall.rs:781`, introduzido em `f8ab32b`),
  que já resolve "esta porta responde, e a quem" — é o núcleo reutilizável.
- **Impacto real:** alto em custo de suporte, nulo em segurança.
- **Esforço:** M
- **Dependências:** RF-NET-05 (a explicação só é correcta se a precedência for
  determinística) e RF-NET-07.
- **Recomendação: IMPLEMENTAR**, estendendo `published_reach` em vez de começar do
  zero.

### RF-NET-13 — Métricas de descarte

- **Estado: PARCIAL**
- **Evidência:** o registo Prometheus existe (`runtime-core/src/metrics.rs`) e é
  exposto em `/metrics` pelo `delonix-mgmt` e pelo `delonix-cri`. **Não há nenhuma
  métrica com labels** — zero `Family<…>` no ficheiro; todas as métricas actuais são
  `Counter`/`Gauge` escalares. A forma `{container, direction, rule_id}` exige
  plumbing novo.
  *A favor*: desde `f8ab32b` **todas as regras emitem `counter`**, e existe já um
  canal para os ler do holder (`fw_counters` → `fwstats`). A fonte de dados está
  pronta; falta a exposição.
- **Impacto real:** médio-alto (diagnóstico), nulo em segurança.
- **Esforço:** M
- **Dependências:** nenhuma para os dois primeiros contadores; RF-NET-04 para
  `drift_total`; RF-NET-01 para `dns_learned_entries`.
- **Recomendação: IMPLEMENTAR**

### RF-NET-14 — Auditoria encadeada

- **Estado: PARCIAL — com conflito de desenho a resolver**
- **Evidência:**
  - `events.jsonl` existe e é append-only (`runtime-core/src/events.rs`).
  - Alterações de política de rede **não emitem eventos**: `grep events:: cmd/firewall.rs`
    → vazio. Falta o requisito antes ainda de falar em encadeamento.
  - **Conflito**: o módulo é explicitamente **sem lock**, e o cabeçalho documenta
    porquê — a atomicidade vem de um `write` `O_APPEND` abaixo de `PIPE_BUF`, o que
    permite N processos efémeros a escrever em concorrência sem `flock`. Encadear por
    hash **obriga a ler a última linha antes de escrever**, o que reintroduz uma
    secção crítica entre processos. Ou se aceita um `flock` (e perde-se a propriedade
    que o desenho protege), ou se usa um esquema que não dependa do predecessor
    imediato (p.ex. assinatura por entrada, ou encadeamento por época com selagem
    periódica).
  - A rotação para `.1` com uma só geração também **quebra a cadeia** por desenho.
- **Impacto real:** médio. Relevante para apresentação em auditoria, não para a
  postura de segurança do datapath.
- **Esforço:** M para emitir eventos; L para o encadeamento com o conflito resolvido.
- **Dependências:** nenhuma
- **Recomendação: REFORMULAR** — separar em dois: (a) emitir eventos de política de
  rede, que é puro ganho e sem conflito; (b) encadeamento, que precisa de um ADR
  próprio a decidir o compromisso com o desenho sem lock e com a rotação.

---

## 3. Lacunas não previstas

1. **Caminho de dados IPv6 paralelo e não filtrado** — ver RF-NET-11. Não é uma
   omissão de v6: é uma SDN v6 **construída de propósito** e deixada sem política.
   É o achado que reordena todo o plano.
2. **`vm bridge` escreve na `FORWARD` do host** (`cmd/vmbridge.rs:116-119,161-164`,
   `iptables -I FORWARD`). Viola a restrição 5. É privilegiado e opt-in, mas é estado
   partilhado que o motor cria e de que ninguém reconcilia.
3. **O anti-padrão nº 6 é o estado actual de todo o crate.** O prompt proíbe
   "shell out para `nft` quando existe ligação netlink"; `delonix-net` invoca o
   binário `nft` em **todos** os caminhos, e faz *parsing* de texto para descobrir
   handles e elementos de mapa. Cumprir o requisito é reescrever o datapath sobre
   netlink — trabalho de dimensão XL, independente dos catorze requisitos. **Tem de
   ser negociado explicitamente**: ou se isenta o código existente e a proibição vale
   só para código novo, ou entra como bloco próprio com o seu próprio orçamento.
   Nota atenuante: as invocações são `Command::new` com argv vectorizado (sem shell),
   e os campos interpolados passam por validadores dedicados (`fw_src_ok`,
   `fw_port_ok`, `fw_proto_ok`, `validate_publish`), pelo que a superfície de injecção
   é menor do que "shell out" sugere. O problema real é a ausência de transacção e a
   fragilidade do parsing, não a injecção.
4. **Estado no netns efémero do holder** — ver RF-NET-04. Toda a política desaparece
   com o holder, e não é reconstruída.
5. **Pods (CRI) e VMs fora do isolamento de namespace** — já registado no `CLAUDE.md`
   como limitação conhecida; relevante porque RF-NET-06 promete "todos os containers
   que satisfaçam o selector" e o CRI atacha por outro caminho.
6. **`l4guard` global e só por manifesto** — ver RF-NET-10.
7. **Desvio de versão anunciada** — `README.rst` diz `0.32.2`, `Cargo.toml` diz
   `0.37.0`. Confirma o deliverable 10.

---

## 4. Sequência de implementação proposta

Diverge da secção 7 do prompt, e a razão é o RF-NET-11.

| Ordem | Bloco | Conteúdo | Porquê aqui |
|---|---|---|---|
| **0** | **Segurança imediata** | **RF-NET-11 (mitigação) + RF-NET-02** | Enquanto o caminho v6 estiver aberto, toda a política é decorativa e qualquer trabalho nos blocos seguintes assenta em falso. Publicar como versão de segurança. |
| 1 | A′ | RF-NET-03, RF-NET-05 (reformulado) | Fail-closed e precedência explícita são a base sobre a qual o modelo novo assenta. |
| 2 | A″ | RF-NET-01 (reformulado) | DNAT do :53, TTL, metadados, scoping por cliente. Independente. |
| 3 | B | RF-NET-07 → RF-NET-06 | Sets nomeados primeiro, selector depois — pela ordem inversa da do prompt, porque 06 sem 07 obriga a reprogramar regras a cada evento (anti-padrão nº 4). |
| 4 | C | RF-NET-12, RF-NET-13, RF-NET-14(a) | Paga o suporte dos anteriores. `explain` estende `published_reach`. |
| 5 | D | RF-NET-04 | Reconciliação, com as métricas de C já disponíveis. Alvo: respawn do holder. |
| 6 | E | RF-NET-11 (paridade completa), RF-NET-10, RF-NET-09(reduzido) | Paralelizáveis. |
| — | Fora | RF-NET-08, RF-NET-14(b) | Cada um precisa de ADR próprio antes de código. |

---

## 5. Critério de aceite da Fase 0 — verificação

| Critério | Estado |
|---|---|
| Catorze requisitos classificados | ✅ |
| Nenhum `INDETERMINADO` nos P0 | ✅ — 01, 02, 03, 04, 05 todos com evidência medida ou lida |
| Teste exploratório de precedência executado e registado | ✅ secção 1.4 |
| Teste exploratório de IPv6 executado e registado | ✅ secção 1.5 e RF-NET-11 |
| Sequência de implementação proposta e justificada | ✅ secção 4 |

**Pontos que exigem decisão do utilizador antes da Fase 1:**

1. **RF-NET-11 primeiro**, à frente do bloco A — confirmar a reordenação.
2. **Anti-padrão nº 6 (netlink)** — isentar o código existente, ou orçamentar a
   reescrita do datapath?
3. **RF-NET-05** — `priority` aditivo com ordem de inserção preservada
   (recomendado), ou reordenação global assumida como *breaking*?
4. **RF-NET-08** — qual das duas saídas rootless, ou modo privilegiado?
5. **RF-NET-09** — aceitar a redução ao sub-requisito 1, dado que a premissa
   principal foi refutada por medição?
