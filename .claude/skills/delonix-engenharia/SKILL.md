---
name: delonix-engenharia
description: Princípios de engenharia de software aplicados ao delonix-runtime — SOLID, design patterns e Clean Architecture como este repo já os pratica (traits de backend, núcleo puro, fronteiras de crate, adaptadores), e sobretudo quando NÃO os aplicar. Usa quando avaliares design/estrutura de código, propuseres uma abstracção, um trait, uma camada ou uma refactorização, ou quando alguém invocar SOLID/patterns/clean arch para justificar uma mudança.
---

# Princípios de engenharia — como este repo já os pratica

O utilizador pediu SOLID/patterns/Clean Architecture **«quando necessário»**, e
essa é a parte que se esquece. Uma abstracção sem segundo consumidor é dívida com
cara de qualidade — e este repo já apagou centenas de linhas dessas.

## A regra número um: nada de código à espera do primeiro chamador

**O anti-padrão mais caro desta base de código**, e repetiu-se **seis vezes**:
uma função pública, bem escrita, com doc-comment, **zero chamadores** — e um bug
latente que só apareceu no dia em que alguém a ligou:

| Símbolo | O que estava errado quando finalmente foi chamado |
|---|---|
| `mount_live`/`unmount_live` | gatava o `setns` por `container.userns`, que não é «está noutro userns» → EPERM em toda a montagem a quente |
| `update_limits` | usava o cgroup ESTÁTICO; dizia «actualizado» e não tocava no cgroup real |
| `set_net_rate` | mesma família |
| `create_with_base` | `--subnet` aceite e descartado, com deriva eterna no reconciliador |
| `publish_port_allow` | **impossível** de funcionar: o tráfego publicado chega todo com o gateway do slirp como origem — a allowlist teria dropado tudo. Foi APAGADO |
| `Net` (22 métodos, 986 linhas) | arquitectura anterior ao holder; mexia na rede do HOST. APAGADO |

**A regra:** um trait/uma camada/um helper novo nasce **com um consumidor real**,
ou não nasce. O `ComputeDriver` (`cmd/workload.rs`) foi feito assim de propósito —
cada método tem caller. E a Fase 2b (promover o trait para o `core`) está
explicitamente adiada **até haver um 2.º consumidor**. Não antes.

**Corolário ao apagar:** `dead_code` só vê itens privados. Ao remover uma API
pública, a cascata privada é do compilador; a pública **conta-se à mão** (foi
assim que saíram `FirewallSummary`, `DnatRule`, `cidr_prefix_len`, `service_vip`).

## Clean Architecture, como está de facto implementada

Não como camadas de manual — como **regra de dependência**, e ela é verificável:

- **Núcleo puro, casca imperativa.** `cmd/reconcile.rs` recebe os dois lados já
  lidos e devolve `Vec<Change>`; **nunca abre um store nem corre um comando**. É
  isso que torna testáveis-como-dados os casos que interessam. O mesmo em
  `bridge_plan`, `resolve_vm_defaults`, `fw_chain_body`, `lower_workload`,
  `build_haproxy_cfg`, `owner()` do `ComputeDriver`.
  **Ao acrescentares lógica de decisão, separa-a do I/O — é o que a torna
  testável sem host.**
- **Dependências apontam para dentro, e mede-se.** UI, proxy e serialização de
  CLI (`ratatui`, `hyper`, `serde_yaml`, `schemars`) ficam confinados ao `-bin`;
  os crates de motor têm de continuar limpos em `cargo tree -e normal -p <crate>`.
  Uma dep nova num crate de motor é matéria de ADR (guarda-rio #4).
- **`delonix-runtime-bin` depende de `delonix-mgmt`, nunca o inverso.** Quando
  isso obriga a duplicar 10 linhas (`dir_size`), **duplica-se de propósito** e
  diz-se porquê. DRY não vence uma fronteira de crate.

## SOLID, traduzido para as decisões que aqui se tomam

- **S — responsabilidade única.** O caso a conhecer é o negativo: `spawn()` tem
  ~405 linhas cobrindo pty, flags de clone, o `clone()`, o handshake de userns, o
  fork do shim, o hook de rede, o cgroup e o `Store::save` — com um comentário
  `// CRITICAL ORDER` a descrever um deadlock que já existiu. Está registado como
  risco de manutenção. **Não a partas por estética**: reordenar blocos ali
  reintroduz um bug que nenhum teste reproduz. Extrair dali é trabalho com
  spike e testes, não uma limpeza de passagem.
- **O — aberto/fechado, e aqui invertido de propósito.** `cmd/exitcode.rs::
  for_error` faz um `match` **exaustivo sem `_ =>`**: uma variante nova de erro
  **pára a compilação** e obriga a decidir. Um catch-all arquivava-a em
  «genérico» sem avisar ninguém. Onde a omissão é perigosa, força o compilador a
  falhar.
- **L — substituição.** Os defaults do `VmBackend` (`manages_own_storage`,
  `auto_selectable`) existem porque um backend REMOTO não cumpre os pressupostos
  do local — o `create_with` resolvia o disco no filesystem local antes de
  perguntar ao backend. **Quando um implementador novo não cumpre a pré-condição
  do trait, o trait é que está errado.**
- **I — interfaces segregadas.** O que um backend remoto não sabe fazer
  **RECUSA-SE a nomear o campo** (hugepages, afinidade de CPU, XML cru do
  libvirt). Aceitar e descartar é a pior falha do catálogo deste repo.
- **D — inversão de dependência.** `VmBackend` + o registo `BACKENDS`, e
  `ComputeDriver` + `ContainerDriver`/`VmDriver`. **Um nome desconhecido é ERRO**,
  com a lista dos registados — o antigo `_ => CloudHypervisorBackend` fazia um
  `stop` de VM libvirt passar pelo caminho errado e deixar o domínio órfão a
  reportar sucesso.

## Patterns que este código usa, e o nome que lhes serve

- **Strategy + Registry** — `VmBackend`/`BACKENDS`. E é explícito que **não é um
  sistema de plugins**: é um mapa populado no arranque. Carregar um `.so` seria
  um ADR novo.
- **Anti-Corruption Layer** — os tradutores de esquema estrangeiro para o
  `RunOpts` interno: `pod_to_run_opts` (Pod k8s), `docker_config_to_run_opts`
  (API Docker), o parser do `compose`. Todos convergem no MESMO tipo interno, e
  é por isso que não divergem em comportamento.
- **Parse, don't validate / typestate** — `AuthoritativeLivePorts::new(...)`
  existe para **obrigar quem chama a afirmar que possui o ingress inteiro**. Um
  `HashSet` cru já foi aceite de um chamador com lista parcial, e as portas
  publicadas passaram a morrer sozinhas. Quando uma pré-condição não pode ser
  esquecida, **codifica-a no tipo**.
- **Single source of truth com dois consumidores** — `fw_rule_tail` é partilhado
  pelo GERADOR e pelo LEITOR: com duas cópias do formato, o leitor deixa de casar
  em silêncio no dia em que o gerador mudar um espaço. Idem `dhcp_lease_ip` (a
  aritmética esteve duplicada em três sítios), `resolve_cap_keep`, `reexec_env`,
  `antispoof_rule_args`.
- **Constante como fonte, com teste a exigir concordância** — `CONVERGING_KINDS`
  decide três coisas escritas em sítios diferentes; divergiram uma vez e o
  sintoma escondeu-se porque o caminho errado também era idempotente. **Se três
  listas têm de concordar, há teste a exigi-lo nos dois sentidos.**
- **Idempotência com rollback na criação** — `create_network` remove o registo se
  o plano físico falhar; senão fica um recurso que `ls` mostra e nada consegue
  usar.

## Quando NÃO aplicar

- **Não extraias um trait para um só implementador.** Espera pelo segundo. O
  `VmBackend` nasceu com dois.
- **Não unifiques dois conceitos porque a struct é parecida.** A 4.ª fusão de
  Kinds **não se fez**: um `kind: Container` com `spec.containers` não é um
  `kind: Pod` de um elemento — o primeiro cria um container `<name>`, o segundo
  cria a netns `pod-<name>` e chama-lhe `<name>-c0`; reescrever partia o DNS e os
  backends de HTTPRoute.
- **Não mudes uma API pública por elegância.** `volumes inspect`/`network inspect`
  ficaram em texto porque migrar para JSON é breaking change numa CLI pública.
- **Não renomeies por coerência.** O pidfile do pin mantém o nome histórico
  `holder.pid` — é o pid que todos os `nsenter -t <holder>` visam.
- **DRY não vence uma fronteira de crate** (ver `dir_size` acima), nem a
  clareza: um `msgid` de i18n partilhado entre dois sujeitos de género diferente
  produz tradução errada em silêncio.
- **Não refactorizes o que não tens como provar.** Sem um teste que falhe com a
  mudança revertida, uma refactorização é uma reescrita com risco e sem rede.

## Como reportar um achado de design

Não «viola SRP». Assim:

> **O quê:** `<símbolo>` faz `<A>` e `<B>`.
> **A consequência medida/possível:** `<falha concreta>` — não «fica difícil de
> manter».
> **O custo de mexer:** o que se parte, que teste falta para o provar.
> **A recomendação:** corrigir agora / registar como dívida com dono / não mexer
> (e porquê).

Dívida **documentada com a consequência** é uma decisão. Dívida escondida é um
bug à espera. Se a mudança move uma fronteira estrutural, o sítio é um ADR —
corre `delonix-adr` e chama o agente `martin` para o C4.
