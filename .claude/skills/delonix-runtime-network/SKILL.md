---
name: delonix-runtime-network
description: Domínio da SDN rootless do delonix — o pin/controlo do netns holder, bridge e slirp, o dataplane nftables (firewall por-container, isolamento de namespace, DNAT de portas publicadas), DNS interno, overlay VXLAN/WireGuard e o proxy L7. Usa quando mexeres em `crates/delonix-net` (sobretudo `infra.rs`), nos grupos `delonix net`/`network` da CLI, ou quando diagnosticares um problema de conectividade, publish de porta ou isolamento.
---

# SDN rootless — o dataplane, e o que já enganou aqui

## O modelo, em três frases

Um processo **pin** (`delonix netns pin`) segura os namespaces e adormece — sem
sockets, sem threads, sem estado. Um processo **controlo** corre lá dentro por
`nsenter` e é reiniciável (socket de controlo, DNS, RA, DHCP). Tudo o que
atravessa a fronteira vai pelo socket de controlo, e o `SO_PEERCRED` valida quem
liga. O pidfile do pin mantém o nome histórico `holder.pid` de propósito: é o
pid que TODOS os `nsenter -t <holder>` da árvore visam.

Ficheiro central: `crates/delonix-net/src/infra.rs`.

## O dataplane nftables, e porque tem a forma que tem

- **`fwguard`** (forward, priority -20) — `169.254.0.0/16` e `127.0.0.0/8`
  negados incondicionalmente.
- **`fwdeny`** (-10) — política de egress por rede.
- **`fwcont`** (-5) — **chain PRÓPRIA** com exactamente 2 regras
  (`ip daddr vmap @fwmap` / `ip saddr vmap @fwmap`), independentemente do número
  de containers. Antes eram 2 regras de jump POR IP POR container: com 49
  containers, ~100 regras por pacote.
- A chain própria **não é cosmética**: no `fwdeny` a ordem relativa às regras de
  egress passaria a depender de QUE COMANDO correu primeiro, não da intenção.

**`accept` não é terminal entre base chains.** É a razão de o isolamento de
namespace viver na chain de CADA workload (first-match terminal ali dentro) e
não numa chain `nsdeny` separada.

**O prologue de estado é por CHAIN, não por IP** (`ct state invalid drop` +
`established,related accept`). Sem ele, um `policy deny` matava o tráfego
legítimo NOS DOIS SENTIDOS: a chain está a `forward priority -10`, ANTES do
`established accept` do `forward` (0), por isso o retorno nunca via o accept.

**`fw_rule_tail` é partilhado pelo GERADOR e pelo LEITOR.** Se cada um tivesse a
sua cópia do formato, o leitor deixava de casar em silêncio no dia em que o
gerador mudasse um espaço. Mesma disciplina do `CONVERGING_KINDS`.

**Um prefixo `/32` é renderizado pelo kernel como endereço nu** — o `/32`
gerado nunca casava com a listagem, e os counters apareciam a `-`.

## O que enganou, e engana outra vez

**Um ficheiro de socket sobrevive ao processo que o criou.** `wait_for_control_sock`
era `path.exists()`; um socket órfão fazia o `ensure_up` anunciar `ingress UP`
sobre um nó SEM plano de controlo. Faz-se `connect`.

**`/sys/class/net` NÃO reflecte a netns do processo** — reporta a de quem MONTOU
o sysfs. De dentro do controlo, aquele directório é o do HOST. Pergunta-se por
netlink.

**`capture()` devolve `Ok` mesmo quando o comando falha** — não olha para o exit
status. Uma sonda com `.is_ok()` é SEMPRE verdadeira. Lê-se a SAÍDA.

**Um `read` que falha não é uma resposta vazia.** `let _ = s.read_to_string(...)`
tornava um timeout indistinguível de uma resposta vazia — e no
`slirp_add_hostfwd` produzia **falso SUCESSO** (resposta vazia não contém
`"error"`). O tecto de 5s no socket de controlo perdia 15 de 30 attaches
concorrentes; com o erro lido e 30s, 30/30.

**`holder_pid.is_some()` não é «o holder é alcançável».** Um upgrade in-place
deixa o holder ANTIGO vivo ligado a um caminho de socket que o binário novo já
não consulta.

**Sempre que uma constante de caminho passar a derivar do `geteuid()`**, grepa
quem a resolve do OUTRO lado de um userns — a 2.ª passagem do re-exec de
`--net <rede>` corre com uid mapeado a 0. `infra::runtime_dir_env()` existe para
isso.

## Publish de portas — o que é verdade

O `slirp_add_hostfwd` liga a `127.0.0.1` por omissão (correcto), e alargar é
opt-in (`-p <ip>:<hp>:<cp>`, `DELONIX_PUBLISH_ADDR`). Só IPv4 — um head não-IPv4
é RECUSADO, nunca descartado.

**A porta que uma regra de firewall tem de nomear é a do CONTAINER**, nunca a do
host: o DNAT corre no `prerouting`, logo o `dport` já é o `cp` quando o pacote
chega à chain.

**Um cliente `127.0.0.1` chega ao container como `10.0.2.2`** (a libslirp não
pode usar loopback como origem dentro da rede emulada), mas **toda a origem
roteável passa intacta** — a filtragem por origem FUNCIONA. Testar uma regra
por-origem com `curl localhost` falha por uma razão que nada tem a ver com a
regra. **Um único cliente de teste não caracteriza um caminho de rede, e o mais
à mão é o caso especial.**

## Antes de mexer

- **Respawnar o holder derruba a SDN de todos os containers do nó.** Numa
  máquina com trabalho a correr, uma correcção dentro do holder só toma efeito
  num respawn — prova por teste e diz que é isso, não finjas validação ao vivo.
- Corre `delonix-runtime-sec` para qualquer mudança em fronteira de rede: o
  isolamento cross-namespace, o `Dependency` e o guarda L4 decidem TODOS pelo IP
  de origem, e já houve dois contornos por multi-homing e um por IPv6.
