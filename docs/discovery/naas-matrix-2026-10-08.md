# 66 — NaaS capability matrix (delonix-runtime, measured 2026-10-08)

Levantamento read-only das capacidades de rede nativa deste motor (`delonix-runtime`), feito
para preparar a base técnica de NaaS/KaaS/CaaS. Âmbito: só NaaS (rede). KaaS/CaaS ficam para
outro levantamento.

**Base medida**: `delonix-runtime` `main` `1cbe9639` (tag `v5.0.0`), worktree
`naas-kaas-caas`. Toda afirmação de estado abaixo foi confirmada no código actual (grep + Read,
ficheiro:linha ou nome de função/teste citado) — o `AGENTS.md` da raiz foi usado só como mapa
de onde procurar, nunca como prova; em pelo menos três secções o `AGENTS.md` estava
desactualizado em relação ao código (ver nota no fim de cada secção onde isso aconteceu — o
exemplo maior é que `GatewayProvider`/`NetworkZoneProvider` já não vivem em `delonix-sdn`:
moveram-se para `crates/contexts/delonix-networking` no ADR-0059, F2a/F2c, e o `AGENTS.md` ainda
os descreve no sítio antigo).

**Fonte autoritativa complementar**: `docs/providers/capability-matrix.md` (gerado por
`delonix provider matrix`, catálogo `1.3.0`, **um teste falha se este ficheiro divergir do
código** — é por isso a fonte mais fiável que existe neste repo para "o que está provado vs. só
escrito") e `docs/proxmox/matrix-9.2.2.md` (gerado por `scripts/proxmox_api_inventory.py`,
mesma garantia). Os dois são citados extensivamente abaixo em vez de reconstruídos.

Estados usados na coluna "Estado" (vocabulário do próprio `capability-matrix.md`, ADR-0050):
**PASS medido** (`supported`: implementado e exercitado — evidência é um check da bateria, uma
secção E2E, um cenário de caos, um teste unitário, ou um teste AO VIVO contra um alvo real),
**PASS por leitura** (implementado, só prova unitária/por leitura — `partial` no catálogo, sem
evidência "live:"), **PARTIAL** (implementado com um limite escrito, ou sem prova nenhuma),
**GAP** (`not-implemented`: zero código), **NOT SUPPORTED** (`unsupported-by-provider`: esse
provider não pode oferecer isto, por desenho).

---

## 1. Matriz de capacidades

### 1.1 Isolamento e segmentação

| Capacidade | Backend(s) | Implementação | Evidência | Estado | Gap concreto | Teste de aceitação sugerido |
|---|---|---|---|---|---|---|
| Redes isoladas (bridge nativa por rede, `kind: Network`) | `linux` (SDN nativa) | `ensure_net_bridge`/`do_attach` (`delonix-sdn/src/infra.rs:2651`, `:3285`) | check:`network: ciclo de vida`; chaos `scen_namespace_isolation` | **PASS medido** | — | — |
| Isolamento lógico por tenant/namespace (`metadata.namespace`) | `linux` | Chain por-workload, guardrail `Deny(OtherNamespaces(ns))` SEMPRE construída pelo IR puro (`delonix-networking::policy::from_container_fw`), renderizada por `policy_nft::chain_body` (`delonix-sdn/src/policy_nft.rs:24`) e `fw_chain_body`/`infra.rs:5101` | teste `fw_body_emits_namespace_isolation_when_no_explicit_ingress`; self-report `NetNamespaceIsolation => Supported{evidence:"chaos:scen_namespace_isolation"}` (`provider_report.rs`) | **PASS medido** | **Assimetria deliberada**: a namespace `default` é "pública" — um workload em `default` só ganha a chain de isolamento quando está noutra namespace que não `default` (`infra.rs:7488`: `if namespace != "default" { apply_firewall(...) }` para VMs). Decisão de desenho documentada (ADR), não um bug — mas um operador de KaaS/NaaS que confunda "default" com "privado" expõe-se. | Container/VM/pod em `default` alcança e é alcançado por qualquer namespace; outro par de namespaces fica mutuamente bloqueado |
| Isolamento entre tenants que partilham o MESMO cluster Proxmox (compute) | `proxmox` | Nenhum — um `SystemContainer`/`VirtualMachine` no Proxmox é `Namespaced::Never` (`delonix-stack/src/kinds.rs:481`): "o provider's node has no namespace of this engine's to put it in — a isolação do motor não chega ao nó do provider (ADR-0058 T7)" | ADR-0058 D1/T7 | **NOT SUPPORTED** (por desenho, declarado) | A isolação lógica por namespace do motor **não existe** no Proxmox — dois VMs/LXC de "tenants" diferentes só se isolam pelo que o PRÓPRIO Proxmox (vnets/firewall de VM/datacenter) fizer. Ver linha "vnet firewall" abaixo para o porquê isto é frágil. | — |
| Prevenção de colisão de sub-rede (CIDR custom, `--subnet`) | `linux` | `validate_subnet` (`delonix-sdn/src/lib.rs:1489`), `Error::SubnetOverlap` (`:1576`, `:1598`), `reserved_overlap` (`:1097`) | teste `two overlapping subnets can both pass the overlap check` (regressão, `:1569`) + testes de `overlaps` | **PASS medido** (unitário; sem check de bateria dedicado encontrado) | Nenhum achado | `network create --subnet 10.50.0.0/24` seguido de um segundo `--subnet 10.50.0.0/25` recusa com `SubnetOverlap` |
| IPAM nativo dentro de um prefixo (alocação, idempotência) | `linux` | `ipam.rs::allocate` (`:177`), `derive_ip_in` (`delonix-net-rules`), `probe_free` (`:211`) — cobre qualquer tamanho de CIDR, não só `/16` | self-report `NetIpam => Supported{evidence:"chaos:scen_concurrent_attach"}` | **PASS medido** | — | — |
| Fuga de leases IPAM (containers/pods/VMs mortos sem `rm`) | `linux` | `reap_orphan_leases` (`ipam.rs:577`), two-pass sob `REF_MARKER_GRACE`, ficheiro `reap-candidates` | testes `primeira_observacao_orfa_nunca_e_reclamada_de_imediato`, `lease_orfao_alem_da_graca_e_reclamado_na_segunda_chamada` | **PASS medido** (fechado — era GAP até 2026-09, documentado no `AGENTS.md` como corrigido) | — | `network ipam prune` |
| IPAM remoto de um provider (Proxmox SDN `pve`, reservas por MAC, drift) | `proxmox` | `ProxmoxIpamProvider` implementa `IpamProvider` (`delonix-proxmox/src/ipam.rs:237`); porta em `delonix-networking::ipam.rs` | **live**: `delonix-proxmox/tests/live.rs::the_ipam_provider_reserves_an_address_and_a_guest_gets_it_by_dhcp` | **PASS medido** (requer lab Proxmox real, opt-in por env var) | — | — |
| IPAM externo (NetBox, phpIPAM) atrás do Proxmox SDN | `proxmox` (via o nó) | — | ADR-0063 D1/D2: "**Accepted (2026-10-06)**, D1 e D2 **não implementados**", nomeados no Sprint 2 de `docs/discovery/66_CONTINUITY_PLAN.md`; lido no PRÓPRIO Perl do nó que `GET /cluster/sdn/ipams/{ipam}/status` recusa tudo menos `pve` e que o plugin phpIPAM tem um `die "not yet implemented"` no caminho que o nó usa para escrever o lease DHCP | **GAP** (ADR Accepted, decisão pendente de implementação) | Com NetBox/phpIPAM registado no Proxmox, o motor **não consegue observar reservas através do nó** — o fluxo de reserva-por-MAC do ADR-0063 D3 só funciona com o IPAM `pve` nativo | live case contra um NetBox/phpIPAM real, assim que D1/D2 entrarem |

### 1.2 Topologia (bridges, VLAN, overlay)

| Capacidade | Backend(s) | Implementação | Evidência | Estado | Gap concreto | Teste de aceitação sugerido |
|---|---|---|---|---|---|---|
| Bridge + veth (rede nativa) | `linux` | `ensure_net_bridge`/`do_attach`/`do_attach_extra` (`infra.rs:2651`, `:3285`, `:3384`) — `ip link add type bridge/veth`, sem opções exóticas | check:`network: ciclo de vida`, chaos `scen_namespace_isolation` | **PASS medido** | — | — |
| VLAN 802.1Q físico | `linux` | `network vlan` (CLI), root-only | self-report `NetVlan => Partial` | **PASS por leitura** | Só interface física do host, dry-run por omissão; sem bateria dedicada | — |
| macvlan / ipvlan | `linux` | **Registado, nunca realizado.** `NetworkStore::create_lan` (`lib.rs:1915`) só escreve um ficheiro declarativo — zero `ip link add type macvlan` em todo o crate (confirmado por grep exaustivo) | self-report `NetMacvlanIpvlan => UnsupportedByProvider`, com `Realized=False` explícito no registo | **GAP** (declarado honestamente — o motor avisa, não finge) | Precisa de `CAP_NET_ADMIN` na init-netns do host, incompatível com rootless-only hoje; por desenho | — |
| Overlay VXLAN (uplink + FDB) | `linux` | `do_vxlan` (`infra.rs:3853`): `ip link add type vxlan` + `bridge fdb append` por peer; `set_vxlan`/`del_vxlan_peer` (`:7239`/`:7266`) validam IP antes do `format!` | self-report `NetOverlayVxlan => Partial`: "inter-node forwarding needs a second node the battery does not have" | **PASS por leitura** (num só nó; multi-nó real não medido) | Tráfego ENTRE nós reais nunca foi medido com pacotes — só a construção local do uplink+FDB | 2 nós reais, VXLAN entre eles, ping cruzando os dois |
| Overlay cifrado (WireGuard) | `linux` | `wg.rs` completo: `keygen`, `ensure_iface` (`:143`), `set_peer`/`remove_peer` (`:201`/`:233`), `validate_peer` (`:285`), chave 0600 | self-report `NetOverlayEncrypted => Partial`: "peer removal proven on one node, traffic across two nodes not measured" | **PASS por leitura** | Mesma lacuna do VXLAN — remoção de peer provada num nó, tráfego real entre 2 nós não medido | 2 nós reais com WireGuard, medir throughput/handshake |
| Segmento remoto (Proxmox Zone + VNet, `kind: Network` + `spec.provider.proxmox`) | `proxmox` | `ProxmoxSegmentProvider` implementa `SegmentProvider` (`network_zone.rs:129`) sobre `sdn.rs` (2146 linhas: `sdn_zones/zone/vnets/vnet/subnet*`, `apply_sdn`) | **live**: `network_zone_provider_owns_by_mark_and_never_pushes_someone_elses_pending_change`, trace real em `docs/proxmox/matrix-9.2.2.md` (85 de 90 rotas `sdn` testadas) | **PASS medido** (lab real, zona `simple` só) | Só a zona `simple` foi exercitada — VLAN/VXLAN/QinQ/EVPN SDN-nativas do Proxmox ficam `partial` no catálogo (`net.segment.remote`) | — |
| `kind: NetworkZone` como forma independente | `proxmox` | **Superseded.** `Form::Sunset(NETWORK)` (`delonix-stack/src/kinds.rs:375`) — ADR-0070 D3: as redes que nomeiam uma zona são fundidas, no `load`, no documento `NetworkZone` que o executor já reconcilia; `kind: NetworkZone` continua a carregar, anunciado como superado | ADR-0070 §tabela, linha `NetworkZone` | **PASS medido** (fold confirmado) | — | — |

### 1.3 Associação de interface a workload

| Capacidade | Backend(s) | Implementação | Evidência | Estado | Gap concreto | Teste de aceitação sugerido |
|---|---|---|---|---|---|---|
| Container/Pod ↔ rede nativa | `linux` | `do_attach`/`do_attach_extra`, multi-homing (`network connect/disconnect`) | check:`update: net-connect a quente` | **PASS medido** | — | — |
| VM (libvirt/Cloud Hypervisor) ↔ rede nativa | `linux` (via backend de VM) | `net_mode: nat\|bridge\|user` no libvirt; attach directo no holder para Cloud Hypervisor; anti-spoof por MAC+IP (`spoofbind_line`, `infra.rs:7359`, cobre qualquer namespace com lease desde 2026-10-02) | self-report `NetStaticIp => Partial` (IPAM), chaos cobre isolamento de VM | **PASS medido**/**PASS por leitura** (mistos, ver linha de IP abaixo) | — | — |
| VM ↔ `kind: Network` da SDN nativa, quando a VM está num nó Proxmox | `proxmox` | **Não existe.** Confirmado directamente no código: `net0_arg` (`delonix-proxmox/src/lib.rs:1762`) resolve SEMPRE para `vmbr0`/bridge do Proxmox; zero referência a `delonix_sdn`/`delonix-sdn` em todo o `delonix-proxmox` fora dos comentários que EXPLICITAMENTE dizem "Not `delonix-sdn`" (`sdn.rs:3-5`, `sdn_lock.rs:1`, `sdn_routing.rs:1`) | `delonix-proxmox/src/lib.rs:3167-3180` (doc-comment): "this engine already has its own firewall ... What follows is a DIFFERENT thing entirely ... has no relationship whatsoever to this engine's SDN firewall — the two never see each other's rules" | **NOT SUPPORTED** (confirmado, não é lacuna por omissão — é a fronteira real hoje) | Ver secção 3 "Combinações de providers" — isto é o achado central dessa secção | — |
| Nó Kind (kubeadm/kind) ↔ anti-spoof com CIDR de pod concedido | `linux` | `spoof_allow_port`/`spoofallow` (`infra.rs:3683`), `allowed_sources`/`source_check_disabled` persistidos, `network_access_rule`/política do nó decide a GRANT (`cmd/policy.rs`) | commit `30d7f584` (DX-6305 refusal numa host sem `br_netfilter`), `96c7ff7b` | **PASS medido** | — | — |

### 1.4 Perímetro, NAT, publish

| Capacidade | Backend(s) | Implementação | Evidência | Estado | Gap concreto | Teste de aceitação sugerido |
|---|---|---|---|---|---|---|
| NAT de saída (masquerade) por rede | `linux` | Toda a rede faz masquerade no uplink do holder | self-report `NetNatSnat => Partial`: "every network masquerades its traffic out ... no battery check names outbound traffic" | **PASS por leitura** | Sem check de bateria dedicado | — |
| Publish de portas (DNAT local, `-p`) | `linux` | `parse_publish_addr` (`lib.rs:450`, suporta `[hostIp:]hostPort:contPort[/proto]`), `expand_publish_range` (`:524`), `can_bind_host_port` (`:657`, bind real, não só sysctl) | check:`update: publish-add a quente` | **PASS medido** | Só `tcp`/`udp` no caminho rootless (slirp) — `SCTP` recusado pelo nome (`Proto::parse`, `:608`), por desenho (ver linha IPv6/SCTP em 2.x do AGENTS.md para o caminho root/CNI, que passa SCTP ao `portmap` sem filtrar) | — |
| Bind address do publish PERSISTIDO (sobrevive a `stop`/`start`) | `linux` | `normalize_publish_spec` (`lib.rs:640`) — fechado 2026-10-07, era a 5ª ocorrência da mesma classe de bug ("estado usado na criação, nunca persistido") | doc-comment `:620-639` | **PASS medido** | — | — |
| NAT via perímetro (SNAT por rede, DNAT/port-forward) | `opnsense` (GatewayProvider) | `OpnsenseNatProvider` (`nat.rs:741`), `NatProvider` port (`delonix-networking::nat.rs:223`) | **live**: `a_source_and_a_destination_nat_rule_load_in_pf_and_are_removed` | **PASS medido** (requer appliance real, opt-in) | Modelo estreito e deliberado: só o que se consegue reler do `pf` (linha de `source_nat`/`d_nat`); duas regras com a mesma rede de origem (ou mesmo proto+porta+alvo) lêem-se como UMA — documentado, não escondido | — |
| IP flutuante / NAT one-to-one | `opnsense`/`linux`/`proxmox` | — | `net.nat.one-to-one`/`net.nat.npt` = `not-implemented` em todos os providers no `capability-matrix.md` (linhas 150-151) | **GAP** | Não há conceito de "floating IP" neste motor hoje — o mais próximo é um DNAT estático via OPNsense (não é 1:1 automático) | — |
| Gateway de perímetro (OPNsense) — ver secção 1.5 detalhada | `opnsense` | — | — | — | — | — |
| L4 Load Balancer / VIP de `kind: Service` | `linux`/`proxmox`/`opnsense` | **Código morto, confirmado e verificado pelo próprio autor.** `do_lbset`/`do_lbclear` (`delonix-sdn/src/infra.rs:4129`/`:4066`), `set_service_lb`/`set_service_lb_algo` (`:5704`/`:5713`) — **zero chamadores** em todo o workspace (`grep -rn "set_service_lb|do_lbset" bins/ crates/` só devolve o próprio `infra.rs`). **E o código tem um bug confirmado DIRECTAMENTE por mim** (não só citado do ADR): `do_lbset` recusa um VIP fora de `is_ingress_ip` (`:4581`, espaço `10.200-10.254`), e `delonix_net_rules::service_vip` (`:441`) devolve SEMPRE `10.90.a.b` — **fora** desse espaço. `service_vip(x)` seguido de `do_lbset(service_vip(x), ...)` falha SEMPRE. | ADR-0066 (Proposed, 2026-10-02): "nothing implemented". Grep: zero `LbProvider`/`trait Lb` em todo o workspace | **GAP** (primitivo de baixo nível existe, morto, com bug confirmado) | Esta é a MESMA família de "função pública morta com bug latente à espera do primeiro chamador" que este repo já catalogou 5 vezes (`mount_live`, `set_net_rate`, `update_limits`, `publish_port_allow`, `Net`) — ver gap #2 na secção 4 | Teste unitário `is_ingress_ip(&service_vip(k))` para uma amostra de `k` (falha hoje); depois, um chaos scenario que mede tráfego real rotando entre backends |

### 1.5 OPNsense — integração ponta a ponta

| Facta | Evidência | Estado |
|---|---|---|
| **Ligação** | `Client::connect` (`lib.rs:233`): `connect_timeout 15s`, `timeout 30s`, `pool_max_idle_per_host 4`, `redirect::Policy::none()` (um 302 nunca é seguido — distingue "sem credenciais" de "credenciais erradas"), TLS verificado por omissão (`insecure_tls` opt-in, `ca_cert_pem` para CA interna). Prova a credencial no `connect` (`GET core/firmware/status`), nunca no 1º uso real. Só `Basic(key,secret)` — sem fallback de password de GUI (deliberado, doc-comment) | **PASS medido** |
| **Descoberta de capacidades** | `capabilities.rs::capability_report` — **declarada, nunca sondada** (contactar a appliance custaria uma mutação implícita); walk exaustivo de `Capability::ALL` sem braço `_`, cada `supported` cita evidência `live:` | teste `only_what_the_live_case_exercises_is_supported` | **PASS medido** (10 capacidades hoje) |
| **Regras/aliases/NAT — criação/leitura/diff/apply** | `ensure_alias`/`ensure_rule`/`remove_alias`/`remove_rule` (`lib.rs:531-723`), drift por `alias_drift`/`rule_drift` (`:1195-1324`); `nat.rs` (916 linhas, dois modelos distintos: `source_nat` flat, `d_nat` nested) | **live**: `a_lowered_policy_lands_on_the_appliance_in_its_order_with_its_fields`, `a_source_and_a_destination_nat_rule_load_in_pf_and_are_removed` | **PASS medido** |
| **Identificação dos recursos geridos (posse)** | `OwnerMark` como categoria firewall `delonix-owner:<token>` (nunca no texto da descrição legível). `ensure_*` **recusa** (`Error::NotOwned`) adoptar por nome/descrição sem a marca — nunca "already present"; `remove_*` só apaga o que tem a marca | **live**: `a_hand_made_rule_is_never_adopted_and_a_hand_made_pending_change_blocks_the_commit` | **PASS medido** |
| **Semântica "reload" vs. "running config"** | `apply()` é **síncrono** — sem UPID/task id, ao contrário do Proxmox (`firewall/filter/apply`/`firewall/alias/reconfigure` são DUAS chamadas separadas e independentes — medido ao vivo que uma regra referenciando um alias recém-criado resolveu correctamente antes de qualquer reconfigure, mas as duas são necessárias para o dataplane realmente pegar). `pending_changes()` (`:764-840`) lê o staged-vs-running comparando `search_rule` com `pf_statistics/rules`/`alias_util` — não há flag "dirty" na API | doc-comment `lib.rs:38-46` | **PASS medido** |
| **Timeouts/retries** | `connect_timeout 15s`/`timeout 30s`; **sem retry automático** de falha de transporte (não há sessão/ticket a expirar como no Proxmox) | — | **PASS por leitura** (comportamento correcto, mas sem retry é uma escolha deliberada, não omissão) |
| **Falhas parciais** | `check_no_foreign_pending` corre ANTES de qualquer escrita; se o `commit` falhar depois, `discard_staged` (`:905-942`) desfaz o que a própria chamada criou (regras antes de aliases) e NOMEIA no erro o que não conseguiu desfazer — nunca finge sucesso | 45 testes de injecção de falhas (`tests/failure_injection.rs`, mock TLS) | **PASS medido** |
| **Preservação de regras externas** | `ensure_*`/`remove_*` nunca tocam numa regra sem a marca deste motor; `check_no_foreign_pending` recusa o `commit` inteiro se há QUALQUER coisa staged que não é nossa (um humano a meio de uma edição na GUI bloqueia o apply, em vez de o motor empurrar por cima) | **live**: mesmo teste de posse acima | **PASS medido** |

**Nota**: todos os testes "live" acima exigem `DELONIX_OPNSENSE_TEST_URL/_KEY/_SECRET` apontando
para uma appliance real; sem isso `tests/live.rs` imprime `SKIP:` e sai — nunca passa em
silêncio (`target_or_skip`, `tests/live.rs:46`). Classifico-os "PASS medido" porque foram
provadamente corridos contra uma appliance real nesta série de trabalho (ver histórico de
commits ADR-0051/0059 F5a), não porque este levantamento os tenha corrido agora.

### 1.6 Firewall / NetworkPolicy

| Capacidade | Backend(s) | Implementação | Evidência | Estado | Gap concreto |
|---|---|---|---|---|---|
| `NetworkPolicy` `scope: container` (default-deny por workload) | `linux` | `policy_nft::chain_body`, dispatch em `cmd/firewall.rs:680,755,804,1482,1607,2293` | self-report `FirewallDefaultDeny => Partial`: "validated live, no battery check" | **PASS por leitura** | Sem check de bateria dedicado a pacotes (só leitura/ordem de regra) |
| `scope: network` (egress por rede, CIDR/FQDN allowlist, rate limit) | `linux` | `cmd/firewall.rs:1689-1728` | self-report `FirewallEgressPolicy => Partial` | **PASS por leitura** | idem |
| `scope: vm` (firewall PRÓPRIA do nó Proxmox) | `proxmox` | `delonix_vm::apply_firewall`→`vm_firewall::apply_on` (`vm_firewall.rs:345-397`); interruptor de datacenter checado ANTES de escrever (`Error::DatacenterFirewallDisabled`→**DX-6508**) | **live**: `a_scope_vm_policy_lands_on_the_nodes_own_firewall_and_reads_back` | **PASS medido** | O live case lê regras/ordem/3 switches de volta — **não mede tráfego de convidado real** (nenhum pacote passou por elas na prova) |
| `scope: systemcontainer` (firewall própria do LXC Proxmox) | `proxmox` | `LxcFirewall` (`vm_firewall.rs:276-330`), `ProxmoxSystemContainerProvider::apply_firewall` (`lxc.rs:1506-1536`) | **live**: `a_system_containers_firewall_is_applied_and_reads_back` | **PASS medido** (mesma ressalva — sem pacote real) | idem |
| `NetworkAccessRule` (regra incremental, com `origin` próprio) | `linux` | fundido com `FirewallPolicy` sem apagar regras doutro documento (`fw.rules.retain(..r.origin.is_some())`) | AGENTS.md secção "kind: NetworkAccessRule" + git log #713 (leituras pelo nome do pod corrigidas) | **PASS medido** | Já corrigido: `delete networkpolicies <c>/<dir>` tinha rc=0 sem remover a regra — fechado |
| Vnet firewall nativo do SDN Proxmox (`.../vnets/{vnet}/firewall/*`) | `proxmox` | Escrito, lido, com regras `forward` por ordem — mas **NUNCA exposto por `delonix` (CLI/Kind)**: zero ocorrências de `sdn_vnet_firewall`/`VnetFirewallOptions`/`vnet_firewall` em `bins/delonix-runtime-bin` ou `delonix-networking` (confirmado por grep) | Medido com pacotes no nó real, documentado no próprio doc-comment (`sdn_routing.rs:16-44`): sob `pve-firewall` (iptables, o default do Proxmox) **as regras são aceites, lidas de volta, e NÃO filtram UM PACOTE** — só filtram sob `nftables: 1` (`proxmox-firewall`) | **PASS por leitura** (função cliente provada), mas **NÃO EXPOSTA** — ver gap #1 na secção 4 | Não há guarda equivalente ao `DatacenterFirewallDisabled` (`scope: vm`) para este caminho — quando alguém o ligar à CLI, vai aceitar e não aplicar em silêncio, salvo adicionar a guarda primeiro |
| `allowSourceCheckOptOut`/`allowedSourcePrefixes` (concessões de anti-spoof) | `linux` | `cmd/policy.rs:234-370`, `apply_source_overrides` (`cmd/container.rs:4322`) | — | **PASS medido** | — |
| Anti-spoofing (MAC+IP, `table bridge dlxspoof`) | `linux` | `SPOOF_TABLE="dlxspoof"` (`infra.rs:3537`), hook `prerouting priority -300`, por PORTA (não por bridge) | ADR-0055 Accepted 2026-10-06 (nota: o `docs/discovery/66_CONTINUITY_PLAN.md` secção 5 ainda lista ADR-0055 como "Proposed with no owner decision" — **inconsistência de prosa no próprio repo**, confirmei no ficheiro do ADR que diz Accepted; cito isto como exemplo vivo de "prosa envelhece", a mesma doença que o AGENTS.md descreve sobre si próprio) | **PASS medido** (desde 2026-10-02/06; ANTES disso era `table ip` e NÃO FILTRAVA NADA — achado crítico já corrigido) | — |

### 1.7 DNS

| Capacidade | Backend(s) | Implementação | Evidência | Estado | Gap concreto |
|---|---|---|---|---|---|
| DNS interno nativo (`<nome>.<ns>.delonix.internal`) | `linux` | `handle_dns`/`dns_server_main` (`infra.rs:8383`/`:8515`), isolamento por namespace no resolvedor, `dns_resolve_multi_for` (round-robin de `kind: Service`) | self-report `NetDns => Partial`: "proven E2E in-session, no battery nslookup" | **PASS por leitura** | Sem check `nslookup` na bateria |
| DNS role num provider remoto (zona SDN Proxmox + PowerDNS, registos escritos pelo NÓ) | `proxmox` | `ProxmoxDnsProvider` implementa `DnsProvider` (`dns.rs:76`); porta em `delonix-networking::dns.rs` | **live**: `the_dns_provider_registers_a_guest_in_the_zones_dns_server`; medido contra PowerDNS 4.9.17 real no lab | **PASS medido** | O motor NUNCA cria o controlador DNS nem guarda a credencial dele (deliberado, ADR-0064 D5) |
| Limpeza de registos DNS deixados pelo nó (gateway de subnet + rename de convidado) | `proxmox` | — | ADR-0064 D4/D6: "decided, **not implemented**" — precisa de live case próprio | **GAP** (decidido, não implementado) | Ver gap #2 na secção 4 |
| DNS autoritativo próprio do motor | — | — | `net.dns.authoritative` = `unsupported-by-provider` em todos | **NOT SUPPORTED** (por desenho — o motor resolve os seus nomes, nunca serve zona de terceiros) | — |

### 1.8 Métricas, MTU, IPv4/IPv6

| Capacidade | Estado | Evidência/Gap |
|---|---|---|
| Métricas de rede (Prometheus) | **PASS por leitura** | `metrics.network.per-workload` = `partial` em todos os providers (`capability-matrix.md`); rx/tx lido por-container pelo coleccionador do dashboard, não exportado por Prometheus; `net.observe` = `partial` (sem comparação com o registo ainda, ADR-0059 F4) |
| MTU — bridges/veth/VXLAN | **GAP, confirmado por mim directamente** | Grep exaustivo em `infra.rs`/`lib.rs`: NENHUM `ip link add` de bridge/veth/VXLAN passa `mtu`. O único MTU explícito do crate é `--mtu=65520` hardcoded para o `slirp4netns` (`lib.rs:2058`/`:4198`/`:4243`, `infra.rs:1892`) — é a NIC virtual do slirp DENTRO do netns do holder, sem relação com a MTU real de uma bridge/overlay. **Sem redução de MTU para o overhead do VXLAN (~50 bytes)** nem exposição ao operador | Nenhuma flag `--mtu` em `network create`; sem PMTUD explícito |
| IPv4 | **PASS medido** | Único stack suportado de ponta a ponta |
| IPv6 | **BLOQUEADO deliberadamente (não "não filtrado")** — ver secção 2 abaixo | — |

---

## 2. IPv6 — estado exacto, re-confirmado directamente no código

O `AGENTS.md` documenta um achado CRÍTICO (v0.37.1, "Bloco 0 do plano 33"): até essa versão, a
SDN atribuía endereço IPv6 ULA a cada container e **a firewall inteira era `table ip` — zero
política em IPv6**, um contorno total do modelo de isolamento sem privilégio. Confirmei
**directamente no código actual** (não só no texto do ADR) que a correcção está em vigor:

- `ipv6_sdn_enabled` (`delonix-sdn/src/infra.rs:3047`) é `false` por omissão.
- **Camada 1** (por container, no attach): `disable_ipv6_argv` (`:3151`), executado dentro de
  `do_attach` (`:3323`) e `do_attach_extra` (`:3420`) — ANTES de qualquer tráfego.
- **Camada 2** (holder-wide, cobre containers já vivos e um privilegiado que remonte
  `/proc/sys`): `disable_ipv6_live` (`:3096`), `table ip6 dlxing` com `forward policy drop`
  (`:3066`, instalada em `:8313`).
- `DELONIX_ENABLE_IPV6=1` é um escape-hatch RUIDOSO (`SECURITY WARNING` no próprio código,
  `:3051`), nunca um default.
- Teste: `ipv6_e_recusado_nas_duas_camadas` (`infra.rs:10421`) confirma as duas camadas.
- Self-report: `NetIpv6 => UnsupportedByProvider` (`provider_report.rs`) — honesto: recusado,
  não "degradado" ou "parcial".

**O que NÃO existe**: suporte real a IPv6 dual-stack (endereçamento, IPAM v6, DNS AAAA,
anti-spoof v6). O **ADR-0065** desenha isso (uma `table inet` única substituindo `table
ip`+`table ip6` de recusa, endereços v6 derivados do lease v4, anti-spoof v6 na família
`bridge`) e está **`Proposed` (2026-10-02), nada implementado** — confirmei por grep que não
existe `table inet` em lado nenhum do crate, só `table ip` (default) e `table ip6 dlxing`
(recusa). O `docs/discovery/66_CONTINUITY_PLAN.md` nomeia-o como item 6.2 do Sprint 6, "nothing
implemented".

**Conclusão**: o achado grave de 2026 (IPv6 como bypass total de política) está **fechado e
confirmado hoje** — é uma recusa activa, correctamente nas duas camadas. O que falta é IPv6 como
CAPACIDADE (não como correcção de segurança), e isso é trabalho novo, não dívida de segurança.

---

## 3. Fronteira de responsabilidade

| Capacidade/decisão | Quem decide hoje | Observação |
|---|---|---|
| Quem é o inquilino/tenant, quotas, billing, IAM, aprovação | **Fora do motor, por desenho** — `docs/discovery/62_NAAS_FASE0_AUDITORIA.md` §0: "o `AGENTS.md` deste repositório proíbe o motor de conhecer um consumidor, um inquilino, um plano ou uma quota" | O `scripts/arch_fitness.py` recusa a compilação se a palavra `tenant`/`NetworkClass` aparecer em `crates/`, `bins/` ou `proto/`. Confirmado por grep: zero ocorrências. |
| `NetworkServiceProfile`/`ProviderBinding` por tenant | **Control plane** (delonix-paas, fora do meu âmbito) | O motor devolve um **relatório de capacidades** (`delonix provider ls`) e aceita um **alvo nomeado** (`providers.yaml`) — nunca escolhe provider por inquilino. |
| IPAM GLOBAL / pools por região | **Control plane** | O motor só sabe do IPAM DENTRO de um prefixo já atribuído a um nó (`delonix-sdn::ipam`) ou dentro de uma zona/vnet já criada num cluster Proxmox. |
| Escritor único/concorrência entre VÁRIOS pedidos (potencialmente de tenants diferentes) sobre o MESMO recurso remoto (zona Proxmox, gateway OPNsense) | **Parcialmente o motor, parcialmente — e com um problema conhecido — o control plane** | O motor TEM um lock distribuído real para mutações do SDN Proxmox (`sdn_lock.rs::acquire_sdn_lock`, token persistido antes de qualquer staging, recuperação de crash por `holder_alive`/`proc_starttime`) e `OwnerMark` para recusar adopção por nome sem marca — isto serializa e protege correctamente MÚLTIPLAS invocações do `delonix-runtime` contra o MESMO cluster/appliance. **Bandeira vermelha, fora deste repo**: o item **C1** do `docs/discovery/62_NAAS_FASE0_AUDITORIA.md` (§7, tabela de sessões) continua **aberto no `delonix-paas`** — "reconciliador sem líder, cliente Proxmox órfão/duplicado, binding não persistido" — segundo o `docs/discovery/66_CONTINUITY_PLAN.md` §6: *"The NaaS programme's item C1 lives in `delonix-paas` and is still open there."* Ou seja: o motor protege-se a si próprio correctamente; o que falta para um NaaS multi-tenant seguro de ponta a ponta é o control plane deixar de ter múltiplos escritores concorrentes sem líder — **isto não é um bug do `delonix-runtime`, é uma dependência implícita que o PaaS ainda não satisfaz**. Cito-o aqui porque o pedido explicitamente quer esta bandeira, não porque haja algo a corrigir neste repo. |
| Política de rede por workload (quem filtra o quê) | **O motor**, com um IR único (`delonix-net-rules::policy::Policy`) e três lowerings testados contra a MESMA tabela de 24 casos (`policy.rs::golden`) — `policy_nft::chain_body` (nft nativo), `delonix_compute::vm_firewall::Policy::from_ir` (Proxmox), `delonix-networking::gateway::gateway_rules` (OPNsense) | Boa notícia: não é "cada backend com tradução divergente" — é literalmente uma IR com três renderizadores testados em conjunto. |
| Compilador de política / IPAM transaccional de UM nó | **O motor** | `docs/discovery/62_NAAS_FASE0_AUDITORIA.md` §0, tabela — confirmado no código (secção 1 acima). |

---

## 4. Combinações de providers

**Pergunta**: o código permite hoje combinar, numa só instância lógica, computação num
provider com rede noutro backend e firewall num terceiro (ex.: VM no Proxmox + rede nativa do
motor + firewall OPNsense)?

**Resposta, confirmada directamente no código (não deduzida)**: **NÃO, para a parte "VM no
Proxmox + rede nativa do motor"**, e **SIM, de forma limitada, para "compute num provider +
firewall NO MESMO provider"**.

- **VM no Proxmox NUNCA entra na SDN nativa deste motor.** `net0_arg`
  (`delonix-proxmox/src/lib.rs:1762`) resolve sempre para a bridge do PRÓPRIO nó Proxmox
  (`vmbr0` por omissão); o `delonix-proxmox` não importa `delonix_sdn` em código nenhum, só em
  comentários que dizem explicitamente o contrário ("Not `delonix-sdn`"). Não há mecanismo
  equivalente ao `vm bridge` (que o `delonix-sdn` já tem para ligar uma VM libvirt à SDN nativa
  via veth privilegiado) para uma VM Proxmox. **Logo "VM no Proxmox + rede nativa do motor" é
  impossível hoje.**
- **"Compute num provider + firewall NO MESMO provider" EXISTE e está testado**:
  `NetworkPolicy` `scope: vm` (VM Proxmox → firewall própria do nó Proxmox, `vm_firewall.rs`) e
  `scope: systemcontainer` (LXC Proxmox → firewall LXC do mesmo nó). Confirmado end-to-end
  (`cmd/firewall.rs:2162-2185`, dispatch por scope) com teste AO VIVO nos dois casos. Isto não é
  "firewall num terceiro provider" — é "a firewall do MESMO provider que já corre o compute".
- **"VM no Proxmox + firewall perimetral OPNsense"**: tecnicamente possível SE a topologia de
  rede (fora do controlo deste motor) encaminhar o tráfego da VM através da appliance OPNsense
  como gateway — mas **o motor não faz nenhuma orquestração automática disto**. O
  `NetworkGateway` (OPNsense) é um recurso de PERÍMETRO independente, sem qualquer ligação
  programática a onde um compute corre; `GatewayProvider`/`NatProvider` da OPNsense não sabem
  nada sobre VMs Proxmox e vice-versa (confirmado: zero referências cruzadas entre
  `delonix-opnsense` e `delonix-proxmox`). A combinação funcionaria só por topologia de rede
  manual do operador (a VM tem a OPNsense como gateway por fora do motor), não por nada que o
  `delonix-runtime` ligue.
- **Container/VM nativo (libvirt/CH) + rede nativa + OPNsense como perímetro de saída**: ESTE
  caso é plenamente suportado — é precisamente o papel do `GatewayProvider`
  (`net.nat.snat`/`dnat`, `net.gateway.filter`) sobre o tráfego que já sai do holder nativo. É a
  combinação "compute nativo + rede nativa + firewall remoto" que FUNCIONA, ao contrário da
  combinação com compute Proxmox.

**Resumo**: a combinação real hoje é "o motor nunca mistura o dataplane do Proxmox com o seu
próprio", e dentro de cada MUNDO (nativo vs. Proxmox) a firewall segue o compute. A única
excepção verdadeiramente cross-provider é compute-nativo + perímetro OPNsense, que é um caso
desenhado de propósito (ADR-0051/0059) e provado ao vivo.

---

## 5. Os 3 gaps mais graves

### Gap 1 — Vnet firewall nativo do Proxmox SDN: aceita e lê de volta, mas NÃO FILTRA nada sob o firewall por omissão do Proxmox (risco: **segurança, falsa sensação de protecção**)

**O que está medido, no próprio código e trace ao vivo**: `crates/providers/delonix-proxmox/src/sdn_routing.rs`,
doc-comment de módulo (linhas 16-44): escrever uma regra em `.../vnets/{vnet}/firewall/*` é
aceite, aparece lida de volta, e **nada a compila** quando o nó corre o firewall por omissão do
Proxmox (`pve-firewall`, iptables) — zero linhas em `iptables-save`/`nft list ruleset`. Só
filtra quando o nó tem `nftables: 1` (`proxmox-firewall`) ligado. Medido com PACOTES reais no
lab (duas vnets, listener em 22 e 23, DROP em 23): sob iptables, **23 aberto em todos os
sentidos** apesar da regra "aplicada".

**Por que é grave e não só uma nota de rodapé**: é exactamente o padrão "aceita, lê de volta,
não faz nada" que separa uma firewall real de uma decorativa — e é feito pelo backend que este
programa NaaS pretende usar para VMs/LXC multi-tenant.

**Mitigante medido, que baixa o risco IMEDIATO**: confirmei por grep que `sdn_vnet_firewall`/
`VnetFirewallOptions` **não são chamados de lado nenhum fora do próprio `delonix-proxmox`** —
zero em `bins/delonix-runtime-bin` ou `delonix-networking`. Não há `kind:` nem comando `delonix`
que exponha isto a um operador hoje. É um método de cliente testado e correcto na API, mas
inatingível pelo utilizador final — a MESMA forma de "função pública sem chamador com um bug à
espera do primeiro consumidor" que este repo já pagou 5 vezes (`mount_live`, `set_net_rate`,
`update_limits`, `publish_port_allow`, `Net`), documentada no próprio `AGENTS.md`.

**Ficheiro/função a corrigir**: `crates/providers/delonix-proxmox/src/sdn_routing.rs`,
`set_sdn_vnet_firewall_options` (linha ~1173) e o caminho de escrita de regras — adicionar uma
guarda que sonde a opção de firewall do nó (`nodes/{node}/config` ou equivalente, o mesmo
padrão de `vm_firewall.rs:353`'s `Error::DatacenterFirewallDisabled`→DX-6508) e recuse a escrita
com um erro novo (ex. `Error::VnetFirewallNotEnforced`) quando o nó não tiver `nftables: 1`.
Esta guarda tem de entrar **antes** de qualquer exposição via CLI/Kind deste caminho, não depois.

**Teste de aceitação**: um live case (ao lado de
`sdn_routing_chain_vnet_firewall_and_the_lock_round_trip_through_the_node`) que (a) desliga
`nftables` no nó, tenta escrever uma regra de vnet firewall, e exige a recusa explícita em vez
do aceite silencioso; (b) com `nftables: 1`, repete a medição por pacotes já feita manualmente
(DROP na porta 23, ACCEPT na 22) como check automatizado — nunca `rc==0` sem olhar para o
tráfego real.

### Gap 2 — Registos DNS deixados pelo nó Proxmox em teardown/rename, sem limpeza (risco: **isolamento entre tenants / apontador obsoleto**)

**O que está medido**: ADR-0064, D4 ("What the node leaves behind is said out loud") — destruir
uma zona com `dns:` deixa o registo `A`/`PTR` do gateway (`<vnet>-gw.<domain>`) no servidor DNS
partilhado; D6 ("Cleaning the leaked records") está **"decided and not implemented"**, e nomeia
também um "guest-rename leak" (renomear um convidado deixa o nome antigo a apontar para o IP
actual). O teste ao vivo existente (`the_dns_provider_registers_a_guest_in_the_zones_dns_server`,
ADR-0064) prova que o nó ESCREVE registos correctamente — não prova a limpeza, porque ela não
existe.

**Por que é grave**: num cluster Proxmox partilhado por vários "tenants" (via o control plane),
um endereço IPv4 reaproveitado por OUTRO workload (possivelmente de outro tenant) pode continuar
a responder a um nome DNS antigo que ninguém removeu, ou um rename pode deixar um nome órfão a
apontar para o endereço actual de um recurso que já não se chama assim. É um apontador stale
clássico que pode cruzar a fronteira de tenant silenciosamente — exactamente a classe de "falha
silenciosa" que a doutrina deste repo (guard-rail 6) existe para proibir, e o próprio ADR-0064
nomeia "Hiding the leaked records" como rejeitado precisamente por essa razão, mas a limpeza em
si ainda não foi escrita.

**Ficheiro/função a corrigir**: `crates/providers/delonix-proxmox/src/dns.rs` — não tem hoje
nenhum caminho de remoção de registo; precisa de uma função nova que fale com o servidor DNS
usando uma credencial PRÓPRIA do operador (`providers.yaml`, entrada `dns:` com `url`/`keyFile`
— a decisão já está escrita no ADR, "never the one the node returns"), chamada do teardown em
`network_zone.rs` (`remove_zone`/`remove_vnet`), e removendo só `<vnet>-gw.<domain>`/PTR que
este motor marcou como seu (mesmo `OwnerMark` que o resto do crate já usa).

**Teste de aceitação**: o live case que o próprio ADR-0064 já nomeia como pendente — destruir
uma zona (ou renomear um convidado) contra o PowerDNS real do lab e confirmar, por uma consulta
DNS real (não pela leitura do registo local do motor), que o registo antigo desapareceu.

### Gap 3 — `netops::remove`: rollback assimétrico deixa um `NetDef` órfão em disco quando a remoção do dataplane falha a meio (risco: **perda/inconsistência de estado, bloqueio de disponibilidade**)

**O que está medido, directamente no código**: `crates/adapters/delonix-sdn/src/netops.rs:88`
(`remove`) apaga o registo declarativo (`store.remove(name)`) **ANTES** do dataplane
(`infra::vxlan_remove`/`infra::network_remove`), e os dois lados do dataplane são **best-effort
e engolem o erro** (`network_remove` devolve `()`, só `tracing::error!`). Isto é o INVERSO da
ordem de `create_bridge` (`netops.rs:27`), que tem rollback simétrico explícito: se o gateway OU
o dataplane falharem, o registo recém-criado é removido. `remove` não tem o equivalente — se
`infra::network_remove` falhar a meio (ex.: `netdef_lock()` indisponível), o `NetDef` físico
fica em disco **sem** nenhum registo `NetworkStore` a apontar para ele.

**Por que é grave**: `network_get` ainda encontra o `NetDef` velho pelo prefixo; um `network
create` SEGUINTE com um prefixo diferente pode bater em `NetworkPrefixConflict`
(`infra.rs:6616`) por causa de um recurso que, do ponto de vista do operador, já não existe (o
`network ls` não o mostra mais). Isto é um recurso de rede preso, silenciosamente indisponível,
sem comando óbvio para o recuperar — a mesma classe "estado necessário para reconstruir o
recurso tem de ser persistido" que este repo já catalogou várias vezes, aqui ao contrário: o
estado necessário para DESFAZER o recurso não sobreviveu à falha.

**Ficheiro/função a corrigir**: `crates/adapters/delonix-sdn/src/netops.rs::remove` (linha 88)
— ou inverte a ordem (dataplane primeiro, registo só depois de confirmado) propagando o erro em
vez de o engolir, ou mantém a ordem actual mas **não remove o registo** quando o dataplane
falhar (deixando o recurso visível em `network ls` para um retry/diagnóstico manual, em vez de
desaparecer da vista do operador enquanto o `NetDef` físico persiste).

**Teste de aceitação**: um teste de unidade/chaos que force `infra::network_remove` a falhar
(lock indisponível/ficheiro bloqueado) e exija (a) que `network ls` CONTINUE a mostrar a rede
(ou um estado "removal failed" explícito), e (b) que um `network create` subsequente com um
prefixo distinto não seja recusado por `NetworkPrefixConflict` vindo de um `NetDef` que já não
tem dono.

---

## Apêndice — fontes primárias usadas

- `docs/providers/capability-matrix.md` (gerado, testado contra o código — `delonix provider
  matrix`, catálogo 1.3.0). **Cell metric: 100 de 252 células aplicáveis provadas = 39,7%**
  (todos os domínios); domínio `network` especificamente: linux 8/28 (28,6%), proxmox 7/27
  (25,9%), opnsense (gateway) 10/22 (45,5%).
- `docs/proxmox/matrix-9.2.2.md` (gerado, testado). **183 de 675 rotas chamadas = 27,1%**; área
  `sdn`: 85 de 90 testadas (0 untested, 5 not-yet).
- `docs/adr/0049`, `0050`, `0051`, `0052`, `0053`, `0055`, `0058`, `0059`, `0063`, `0064`,
  `0065`, `0066`, `0070`.
- `docs/discovery/62_NAAS_FASE0_AUDITORIA.md`, `docs/discovery/66_CONTINUITY_PLAN.md`.
- Código: `crates/adapters/delonix-sdn`, `crates/contexts/delonix-networking`,
  `crates/foundation/delonix-net-rules`, `crates/providers/delonix-opnsense`,
  `crates/providers/delonix-proxmox`, `crates/interfaces/delonix-node-api`,
  `bins/delonix-runtime-bin/src/cmd/{firewall,network,network_gateway,network_zone,policy,resource}.rs`,
  `crates/contexts/delonix-stack/src/kinds.rs`.
