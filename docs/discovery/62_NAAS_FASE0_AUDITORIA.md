# 62 — NaaS / Network Provider: Fase 0 e Fase 1 (auditoria antes de código)

Data: 2026-09-27. Base medida: `delonix-runtime` `origin/main` `a4d671a4` (v4.4.0 + 65),
`delonix-paas` `origin/main` `15fd72e`. Documento canónico que rege isto:
`PROMPT_MASTER_DELONIX_RUNTIME_NETWORK_PROVIDER_NAAS.md` (fornecido pelo dono, 2026-09-27).

Este documento cumpre a §42 desse prompt: inventário, fluxo real, ownership, capability matrix,
bypasses, gaps P0–P3, primeiro incremento, comandos de validação e perguntas bloqueadoras.
**Não há código neste passo.** Cada afirmação traz o sítio; «estático» quer dizer lido no
código e ainda não reproduzido; «verificado» quer dizer medido.

## 0. Tradução do prompt para a fronteira deste repositório

O prompt descreve um NaaS com tenant, IAM, quotas, billing, NetworkClass e aprovações. O
`AGENTS.md` deste repositório proíbe o motor de conhecer um consumidor, um inquilino, um plano
ou uma quota. As duas coisas conciliam-se assim, e é esta leitura que decide o que se
refactora aqui:

| Conceito do prompt | Onde vive | No vocabulário do motor |
|---|---|---|
| API tenant, IAM, quotas, NetworkClass, aprovação, billing | control plane (outro repositório) | — (o motor não os conhece) |
| NetworkServiceProfile, ProviderBinding persistido por recurso tenant | control plane | o motor devolve o **relatório de capacidades** e aceita um **alvo nomeado**; não escolhe por inquilino |
| Contratos de provider por papel, capability discovery, validate/plan/apply/observe/verify | **motor** | portas em `delonix-sdn`/`delonix-compute`, Kinds de rede, `delonix provider` |
| Compilador de política (IR → nft / Proxmox / OPNsense) | **motor** | `ContainerFw`/`FwRule`, `NetworkPolicy`, `NetworkAccessRule` |
| IPAM transaccional de um nó | **motor** | `delonix-sdn::ipam` |
| IPAM global, pools por região | control plane | — |

Consequência prática: nenhuma sessão delegada a partir deste repositório escreve
`tenant`, `NetworkClass` ou nomes de repositórios consumidores em `crates/`, `bins/` ou
`proto/` (o `scripts/arch_fitness.py` recusa-o).

## 1. Inventário dos componentes de rede

### 1.1 Motor (`delonix-runtime`)

| Peça | Sítio | Papel |
|---|---|---|
| holder (pin + control + slirp), veth/nft, DNS, DHCP, RA, NetDef, rotas, services | `crates/adapters/delonix-sdn/src/infra.rs` | dataplane rootless |
| IPAM por prefixo | `delonix-sdn/src/ipam.rs` (`allocate` 157, `reserve` 213, `release` 255, `reap_orphan_leases` 398) | leases `id→ip` |
| `NetworkStore`, slirp por container | `delonix-sdn/src/lib.rs` | registo declarativo |
| CNI | `delonix-sdn/src/cni.rs` | rede de pod do CRI |
| WireGuard, bpf | `delonix-sdn/src/{wg,bpf}.rs` | overlay cifrado, fluxos |
| regras puras (`Cidr`, `bridge_name`, IPAM em prefixo) | `crates/foundation/delonix-net-rules` | sem dependências |
| portas `NetworkProvider`, `VmNetwork` | `crates/contexts/delonix-compute/src/ports.rs:110,128` | attach/detach/publish/firewall |
| porta `GatewayProvider` | `delonix-sdn/src/gateway.rs:126` | alias/regra/commit de perímetro |
| porta `NetworkZoneProvider` | `delonix-sdn/src/network_zone.rs:66` | zona/VNet remota |
| `VmBackend::apply_firewall/read_firewall` | `delonix-compute` (porta de VM) | firewall por VM remota |
| OPNsense | `crates/providers/delonix-opnsense` | `GatewayProvider` (ADR-0051) |
| Proxmox SDN + firewall de VM | `crates/providers/delonix-proxmox/src/{sdn,network_zone,vm_firewall}.rs` | `NetworkZoneProvider`, ADR-0052 |
| libvirt | `crates/adapters/delonix-vm` (`ensure_libvirt_network`, `net-update`, nwfilter) | rede `default`, reservas DHCP |
| catálogo de capacidades | `delonix-compute` `capability.rs` (ADR-0050) | `delonix provider ls/describe/matrix` |
| Kinds de rede | `Network`, `NetworkRoute`, `NetworkPolicy`, `NetworkAccessRule`, `Dependency`, `Service`, `IPPool`, `HTTPRoute`, `Ingress`, `Gateway`, `NetworkGateway`, `NetworkZone` | `crates/contexts/delonix-stack/src/kinds.rs` |
| API local | `crates/interfaces/delonix-mgmt/src/lib.rs:249-265` (`/v1/net/firewall`, `/egress`, `/attach-extra`) | congelada pelo ADR-0041 D4 |
| OpenStack | **não existe código** — ADR-0039 `Proposed`; `type: openstack` é recusado em `providers_config.rs:570` | — |

### 1.2 Control plane (`delonix-paas`, só leitura)

| Peça | Sítio | Nota |
|---|---|---|
| modelo NaaS e portas `NetworkProvider`/`NetworkBackend` | `crates/delonix-net-model/src/{provider,backend}.rs` | próprio, paralelo ao do motor |
| API `/v2/net/*` | `delonix-api/src/netapi*`, registada em `ui/startup_reconcile.rs:544-576` | tenant |
| store KV (bucket `net`) | `delonix-net-store/src/repo_core.rs:7` | desired state |
| reconciliador | `net_reconciler.rs:55-130` (sem eleição de líder), `net_sweeper.rs` | |
| adaptadores | `delonix-net-linux` (usa o motor v3.1.0), `delonix-net-libvirt` (virsh), `delonix-net-proxmox` (**órfão**), `delonix-image-proxmox` (2.º cliente Proxmox) | |
| pin do motor | `Cargo.toml:50,82-90` → `rev 6680745` = **v3.1.0** | a v0.56.0 só num ramo local |

## 2. Fluxo real (hoje)

```
control plane /v2/net ──► reconciliar ──► NetworkProvider (env DELONIX_NET_PROVIDER, OnceLock)
      │                                   └► delonix-net-linux ──► motor v3.1.0 infra::network_create
      │                         e SEMPRE a seguir ► DelonixNetBackend (networks_subnets.rs:790)
      ├─ compute_sched ──► delonix-net-libvirt (virsh, fixo, fora do funil)      [2.ª realização]
      ├─ ui/* e overlay ──► delonix_net::infra::* directamente                 [bypass da porta]
      └─ timer 30 s ──► reap_orphan_hostfwds(live só do store do control plane) [escritor concorrente]

motor (CLI / stack apply / node API)
      ├─ Kind Network ──► NetworkStore + NetDef ──► holder (bridge preguiçosa no 1.º attach)
      ├─ NetworkPolicy/NAR/ingress/egress ──► registo do container (flock) ──► nft -f (atómico)
      ├─ delonix-mgmt /v1/net/firewall ──► nft SEM registo                      [2.º escritor]
      ├─ NetworkGateway ──► GatewayProvider (native recusa | OPNsense)
      └─ NetworkZone ──► NetworkZoneProvider (Proxmox; escolhido pela CONTAGEM de providers)
```

## 3. Ownership por recurso (um escritor, ou o conflito que existe)

| Recurso | Escritor(es) hoje | Veredicto |
|---|---|---|
| regras nft por container | registo+nft (CLI/Kinds) **e** `delonix-mgmt` (só nft, só IP primário) | **conflito** — mgmt passa ao lado do registo |
| hostfwds do slirp de ingress | motor **e** control plane (timer 30 s, lista parcial) | **conflito P0 cruzado** — a mesma classe do apagão documentado em `infra.rs` |
| holder / tabelas `dlxing` | binário do motor **e** `delonix-engine` do control plane (re-exec como holder) | conflito se partilharem o root (não verificado) |
| NetDef (`ingress/networks`) | `network_create` (com lock); `network_create_with_gateway`, `network_remove`, `update_netdef_egress` **sem lock** (o último no processo do holder) | **conflito interno** |
| alocação de /16 | `NetworkStore::create` (10.200–254) e `infra::network_create` (10.201–254) não se vêem; `validate_subnet` aceita sobrepor 10.200/16 e 10.0.2/24 | **conflito interno** |
| leases IPAM | `ipam` (flock) **e** DHCP de VM por hash de MAC fora do IPAM (`dhcp_lease_ip` 2400) | duas autoridades |
| `httproute/manual.json`, `config.json` | `update_manual` read-modify-write sem lock; `fs::write` não atómico | race |
| SDN do cluster Proxmox | `apply_sdn` (`PUT /cluster/sdn`) do motor **e** `delonix-net-proxmox` do control plane; ambos aplicam TUDO o que estiver pendente, incluindo edições de operador | conflito latente |
| objectos OPNsense | motor; identidade = nome/descrição; `commit` aplica pendentes de terceiros | adopção por nome |

## 4. Capability matrix actual (papel × provider)

I = implementado e alcançável por Kind/CLI; P = parcial; L = só biblioteca + teste ao vivo; A = ausente.

| Papel | Linux nativo | Proxmox | OPNsense | libvirt | Cloud Hypervisor | OpenStack | CNI |
|---|---|---|---|---|---|---|---|
| fabric/overlay | P (VXLAN/WG, um nó) | L (fabrics, sdn.rs:1046-1317) | A | A | A | A | — |
| segmento | I (bridge no holder) | I/P (`NetworkZone`: zona+VNet) | A | P (`default`, nat/bridge/user) | I (tap) | A | I (bridge CNI) |
| porta | I (`NetworkProvider::attach`) | P (`net0`; VM→VNet não ligada) | A | P (extraNics só XML) | I | A | I |
| IPAM | I | L (`create_sdn_ipam`, `vnet ips`) | A | P (`ip-dhcp-host`) | I (derivado do MAC) | A | I (host-local) |
| subnet | P (`Cidr`) | L | A | A | A | A | — |
| rota | I (`NetworkRoute`) | L (só leitura) | A | A | A | A | — |
| gateway/perímetro | P (`native` recusa tudo) | A (excluído, ADR-0049 D3) | I (alias+regra, sem update) | A | A | A | — |
| firewall por workload | I (nft) | I/P (`scope: vm`) | A | P (anti-spoof nwfilter) | P (anti-spoof no tap) | A | — |
| NAT | I (masquerade, DNAT publish) | A | A | I (rede NAT) | via SDN | A | portmap não ligado em root |
| LB | P (`Service` por DNS) | A | A | A | A | A | — |
| DNS | P (`*.delonix.internal`) | L (`create_sdn_dns`) | A | A | A | A | — |
| telemetria | P | L | A | A | P | A | — |
| IPv6 | recusado por omissão (fail-closed) | — | — | — | — | — | — |

O catálogo (ADR-0050) **não tem** entradas `gateway.*`, `nat.*`, `lb.*`, `dns.*` além de `net.dns`,
e o `ProviderKind` só conhece Compute/Network/Storage/Image. O OPNsense não aparece no
`delonix provider ls`, e o `providers.yaml` (ADR-0054) não configura providers de rede.

## 5. Bypasses e duplicações

1. `delonix-mgmt /v1/net/firewall|egress|attach-extra` escreve nft sem o registo do container.
2. Três clientes Proxmox no ecossistema (motor `delonix-proxmox`; control plane
   `delonix-net-proxmox`, órfão, e `delonix-image-proxmox`, TLS sempre desligado).
3. O control plane chama `infra::*` do motor directamente fora da sua própria porta (≥20 sítios).
4. O control plane realiza a mesma rede duas vezes (provider **e** `DelonixNetBackend`; e libvirt
   pelo `compute_sched`, sem delete).
5. Duas portas `NetworkProvider` com o mesmo nome e contratos diferentes (motor e control plane).
6. Funções públicas sem chamador em produção: `set_service_lb`/`clear_service_lb`,
   `import_iptables`, `alloc_ip`, `container_ip6`, `firewall_summary`, `network_routes_live`,
   `create_with_base`; e toda a SDN Proxmox além de zona/VNet (subnet, IPAM, DNS, fabric, IPs).

## 6. Gaps priorizados

### P0 — fail-open ou perda de isolamento

| # | Achado | Sítio | Estado |
|---|---|---|---|
| P0-1 | **Egress da origem contornado.** `fwcont` faz `ip daddr vmap @fwmap` antes de `ip saddr vmap @fwmap`, e os elementos são `jump`. Um `accept` na chain do DESTINO termina a base chain e a chain da ORIGEM nunca é avaliada: um `egress deny` fica sem efeito para destinos que aceitam. | `infra.rs:775-778`, `4387` | estático, semântica nft confirmada; a reproduzir |
| P0-2 | Regras não `nft_safe` são **saltadas em silêncio** no holder: um `deny` inválido desaparece e o tráfego cai na política (que pode ser `allow`). | `infra.rs:4112-4115` | verificado no código |
| P0-3 | Isolamento de namespace é **aviso** e não erro na criação e no `start`. | `compute/network.rs:88`, `container.rs:3698` | estático |
| P0-4 | `ingress rm/clear` que esvazia as regras faz `clear_firewall` e o container **perde o isolamento de namespace**. | `firewall.rs:656,1168` | estático |
| P0-5 | Redes com CIDR fora de 10.200–10.254: o holder recusa `firewall`/`ns_set_join` (`is_ingress_ip`) e esses containers ficam **sem isolamento**. | `infra.rs:3825` | estático |
| P0-6 | `delonix-mgmt` escreve firewall sem registo e só no IP primário — o próximo reapply desfaz, e o multi-homed fica aberto. | `mgmt/lib.rs:249-265,1098-1142` | estático |
| P0-7 | (control plane) timer de 30 s a reapar hostfwds com lista parcial. | `delonix-paas startup_reconcile.rs:279-307` | estático; mesma classe do apagão já medido |

### P1 — fugas, corridas, adopção por nome

- IPAM: lease de rede CIDR gravado em `<cidr>.json` e libertado em `10.X.json` (`allocate` usa o
  prefixo cru, `release` via `key_for_ip`→`registry_key`) → **fuga de lease**; `reserve` fail-open
  (213); o stop liberta o lease contra a premissa do `restore_lease`; leases de redes extra não
  libertados; DHCP de VM fora do IPAM.
- NetDef sem lock em 3 escritores; dois alocadores de /16 que não se vêem.
- `network rm` não recusa com containers ligados; `do_netdel` não tira `@dlxbr`/`@netpair` nem
  pára o DHCP; `publish_port` e `vm_attach` sem rollback; `httproute` sem lock/atomicidade.
- OPNsense: identidade por descrição (regra manual «adoptada» e depois apagada), `commit` aplica
  pendentes de terceiros, sem update nem deriva de conteúdo.
- Proxmox SDN: `apply_sdn` recarrega pendentes do operador; `ensure_*` adopta por nome.
- Linha de controlo do holder: `ip`/`gateway` do `attach`, `burst` do `netrate`, campos do
  `wg-up`/`wg-peer` sem validação; `cni::resolve_plugin` aceita `/` e `..`; `vm bridge
  --vm-subnet` sem validação e sem verificar root antes do `--apply`.

### P2 — contrato e capacidades

- Sem portas por papel para IPAM/DNS/LB/subnet/fabric remotos; sem `plan`/`observe`/`verify`
  uniforme; sem envelope de erro estável (`unsupported_capability`, `stale_plan`, `partial_apply`…).
- Catálogo sem `gateway/nat/lb/dns`; OPNsense fora do catálogo e do `providers.yaml`;
  `NetworkZoneProvider` escolhido por contagem e não por nome; node API não publica gateway/zona.
- `NetworkGateway`/`NetworkZone` sem cobertura na bateria E2E.
- ADR-0024 (`selector` na política) aceite e por implementar; ADR-0013 com IPAM/`Cidr` por ligar.

### P3

- IPv6 (a recusa actual é correcta); OpenStack Neutron (ADR-0039 Proposed, zero código); LB real;
  DNS autoritativo; topologia; SLOs.

## 7. Refactorização decidida por este documento, e a quem vai

Cada linha é uma sessão local, num worktree próprio em
`~/workspace/ngolacloud/.worktrees/<repo>/<tarefa>`, um PR por sessão.

| Sessão | Repo | Âmbito | Precisa de aprovação antes de código? |
|---|---|---|---|
| **S1 fail-closed do firewall** | motor | P0-1…P0-5: separar o dispatch em duas base chains (saída e entrada) para `accept` não ser terminal; recusar o ruleset inteiro quando uma regra não é `nft_safe`; isolamento de namespace como erro; `rm/clear` preserva o isolamento; isolamento em redes CIDR | não — corrige comportamento, não muda ownership |
| **S2 IPAM e NetDef transaccionais** | motor | chave de lease única, `reserve` fail-closed, leases de redes extra, lock nos escritores do NetDef, um só alocador de /16, `validate_subnet` contra os prefixos reservados | não |
| **S3 ciclo de vida e escritor único do dataplane** | motor | `network rm` com dependentes, `do_netdel` completo, rollback de `publish_port`/`vm_attach`, lock+atomicidade no `httproute`, `delonix-mgmt` a passar pelo registo ou a recusar (P0-6) | não para o motor; a rota do mgmt toca no ADR-0041 D4 — decidir recusar em vez de alargar |
| **S4 superfície de privilégio** | motor | validar todos os tokens da linha de controlo do holder, `cni::resolve_plugin`, `vm bridge` | não |
| **S5 contrato de provider de rede (ADR)** | motor | ADR novo: portas por papel, relatório de capacidades de rede/gateway, lifecycle validate/plan/apply/observe/verify, erros estáveis, selecção por nome, `providers.yaml` para providers de rede. **Só o ADR e o spike**; implementação depois de aceite | **sim** (muda fronteira) |
| **S6 posse nos providers remotos** | motor | marca de posse imutável no OPNsense e na SDN Proxmox, recusa de adopção por nome, pré-condição «sem pendentes alheios» antes de `commit`/`apply_sdn`, E2E de `NetworkGateway`/`NetworkZone` | não; coordenar com a sessão activa «Proxmox API interface assessment» (worktree `sdn-rest`) |
| **C1 escritor único no control plane** | control plane | P0-7, realização dupla, `compute_sched` fora do funil, binding não persistido, reconciliador sem líder, cliente Proxmox órfão/duplicado, pin do motor | **sim** (ownership) |

Ordem: S1 e S4 primeiro (isolamento); S2 e S3 em paralelo (ficheiros diferentes: `ipam.rs`/NetDef
contra `network.rs`/`httproute`/mgmt — **S2 e S3 tocam ambos `infra.rs`**, por isso S3 rebaseia
sobre S2); S5 depois de S1 (o ADR cita o IR corrigido); S6 depois de S5 aceite na parte do
contrato, mas a marca de posse pode começar já.

## 8. Primeiro incremento vertical proposto

«Rede privada isolada no provider Linux, fail-closed»: `kind: Network` + dois workloads em
namespaces diferentes + `NetworkPolicy` de saída `deny` + `NetworkAccessRule` de entrada.
Critério de saída: `stack apply` → `stack plan --detailed-exitcode` = 0; tráfego permitido passa,
cross-namespace e o egress negado **não** passam (medido com pacotes, contadores nft lidos);
`stack destroy` → zero leases, zero elementos `@fwmap`/`@dlxns`, zero NetDef. É exactamente o
que S1+S2+S3 fecham, e passa a check da bateria (`scripts/e2e.sh`) e cenário de caos.

## 9. Comandos e ambientes de validação

```bash
cargo test -p delonix-sdn -p delonix-net-rules -p delonix-opnsense -p delonix-proxmox -p delonix-compute
python3 scripts/arch_fitness.py && python3 scripts/lang_ratchet.py
OUT=/tmp/dlxn DELONIX_ROOT=$OUT/root DELONIX_NET_RUNTIME_DIR=$OUT/net scripts/e2e.sh   # os DOIS roots
scripts/chaos.sh                                                                       # isolamento
```

Resultado medido nesta auditoria (worktree `naas-audit`, `RUSTC_WRAPPER=` vazio):
**19 suites, 484 testes, 0 falhas, 0 ignorados** em `delonix-sdn`, `delonix-net-rules`,
`delonix-opnsense`, `delonix-proxmox` e `delonix-compute` (unitários, failure-injection e
doc-tests). Os testes ao vivo do Proxmox/OPNsense não foram corridos (precisam do
lab). Nada disto prova P0-1: os testes existentes não medem egress cruzado com um destino que
aceita, e é esse o primeiro teste que S1 escreve.

Ambientes: host de desenvolvimento com os dois roots isolados (nunca o root real — há produção
viva); lab Proxmox `pve`/`pve2` e appliance OPNsense para S6; nenhum ambiente OpenStack existe.

## 10. Perguntas bloqueadoras

1. **C1**: o control plane deve passar a consumir as portas do motor (e deixar os seus adaptadores
   `delonix-net-*`), ou manter os seus e o motor fica só como dataplane de nó? Decide quem é o
   escritor do SDN Proxmox e do OPNsense.
2. **S3/mgmt**: as rotas de rede do `delonix-mgmt` recusam (fail-closed) até o `delonix-node-api`
   as substituir, ou passam pelo registo? (ADR-0041 D4 congela a superfície.)
3. **S5**: o relatório de capacidades de rede entra no catálogo existente (ADR-0050, novo
   `ProviderKind::Gateway`) ou num documento separado por papel?
4. **OpenStack**: fica fora até haver ambiente (recomendado), ou abre-se o spike do ADR-0039?
