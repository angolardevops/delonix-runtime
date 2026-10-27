# Tecto honesto da matriz de capacidades — porque 270/270 não é alcançável

**Data:** 2026-10-27 · **Estado medido:** 180/270 células provadas (66.7 %, nível N2)

> **Bucket B +2 (2026-10-27): `host.capacity` CH + libvirt graduado.** A semântica já estava fixada pelo Proxmox (`host.capacity` supported = o nó reporta CPU/memória/disco); para CH e libvirt o "nó" é o HOST LOCAL onde as VMs correm. CÓDIGO real: `delonix_node::host::capacity(disk_path) -> HostCapacity` (cpus+model de /proc/cpuinfo, mem/swap de /proc/meminfo, disco via statvfs; disciplina «nunca um zero calado» — o que não se mede nomeia-se em `unmeasured`). Cada provider expõe um `host_capacity(vmdir)` fino + um teste no SEU crate (`host_capacity_reports_the_local_host`) que lê o host real e prova os invariantes (cpus≥1, mem_total>0, usado≤total p/ mem/swap/disco). Arms citam `test:…`. 7 unit tests verdes (4 em delonix-node + 1 por provider + parsers). Gates/ratchet verdes.

> **Bucket B iniciado (2026-10-27):** graduei `metrics.prometheus` (CH) — descobri que as séries per-domínio de VM (`delonix_workload_{cpu_usage_seconds,memory_current_bytes}{kind="vm"}`) JÁ existiam (lidas do cgroup leaf do VMM em `collect_workload_series`); o detalhe «vms_running/total only» estava obsoleto. Prova = secção e2e `metrics: /metrics exports per-VM cpu and memory series for a Cloud Hypervisor VM` (2/2 live: mem=104 MB, cpu=0.51s para `kind="vm"`). Trabalho de CÓDIGO real: implementei `vm_tap_net_bytes` (delonix-sdn: lê o rx/tx do tap da VM no netns do holder, swap host→guest) + wiring em `dashstats::collect_workload_series` + teste unitário do parser — mas `metrics.network.per-workload` (CH) FICA `partial`: não consegui green ao vivo porque o `ingress/holder.pid` não resolvia neste estado do host (o mesmo que deixou o container-net ausente na corrida isolada). Honesto: código entregue e unit-testado, capability não-afirmada sem prova end-to-end. `host.capacity` e libvirt `metrics.prometheus` por fazer (libvirt não tem pid do qemu no registo → cgroup_missing, visto ao vivo no `pve92`).
· **Fonte:** `delonix provider matrix` + `delonix provider describe <p> -o json` (binário de `bins/delonix-runtime-bin`, árvore `develop`).

> **Bucket A — ENCERRADO (2026-10-27):** graduadas **7 células**, todas provadas ao vivo:
> - **libvirt** `vm.destroy` + `vm.static-ip` + `vm.ip.observed` — uma bateria e2e (`vm: a libvirt VM boots at its reserved static IP…`, 7/7 live, uma VM);
> - **libvirt** `vm.cpu.pinning` — `vm: a libvirt VM's vCPUs are pinned to the requested host cores, live` (3/3 live: `virsh vcpuinfo` → afinidade `yy---…` nos 2 vCPUs, só cores 0-1 de 20);
> - **cloud-hypervisor** `access.transport-verified` — `vm: the Cloud Hypervisor control transport is a per-VM unix socket…` (4/4 live: socket unix, uid do motor, mode 700, sem TCP);
> - **linux** `access.credential-in-vault` — `access: a --secret is injected from the vault at spawn but never persisted…` (6/6 live: valor presente em runtime, 0 ocorrências do plaintext no registo/inspect, secret inspect redige).
>
> **Reclassificações da triagem ao vivo** (o 1º rascunho estava otimista): `vm.restart-policy.native` → **B** (crash→restart exige device `<panic>`/pvpanic inexistente); `vm.network.bridge` → **C** (enslave de NIC a bridge = root); **`access.transport-verified` (linux) → fica `partial`** — ACHADO: a afirmação antiga "no TCP is ever bound" é **falsa**; o processo `delonix-cri` liga um TCP de **loopback** `127.0.0.1` (o streaming server do CRI p/ exec/attach/port-forward). É local-only (nunca 0.0.0.0) e os sockets de controlo são unix+uid+mode 600, mas decidir se o streamer de loopback conta como "supported" é chamada do dono — não flipei; corrigi o detalhe para refletir a realidade. Bucket A real = 7 feitas + 1 que fica honestamente partial.

> A matriz é **provada, não afirmada**. Os portões `cmd::provider::tests::every_supported_capability_cites_evidence_that_exists` e `the_published_matrix_is_the_generated_one` recusam qualquer `supported` cuja evidência (`check:`/`e2e:`/`chaos:`/`test:`/`live:`) não exista mesmo no repositório. Flipar uma arm sem prova real **chumba o build** — não é uma opção.

## Resumo executivo

Das ~96–99 células ainda não provadas, **23 NÃO devem mudar nunca** (desenho, segurança ou proveniência honesta). Logo o **máximo honesto absoluto é 270 − 23 = `247/270` (91.5 %)**, e só se *todas* as de engenharia e infra forem feitas. **270/270 é categoricamente impossível** sem mentir na evidência.

Repartição das não-provadas (classificação curada célula-a-célula no fim):

| Bucket | Nº | O que são | Alcançável? |
|---|---:|---|---|
| **A — Só-falta-prova** | 9 | A funcionalidade já existe e funciona num provider utilizável neste host; falta só a bateria citada (foi o caso de `vm.migration.cold`). | **Sim, já** — sem código novo |
| **B — Engenharia** | 11 | Feature incompleta; exige código novo real (exportar métricas, probes de dataplane, selectores de label). | Sim, mas é trabalho de código |
| **C — Infra** | 53 | Precisam de infra ausente: PVE vivo (37), OPNsense vivo (12), root + dispositivo PCI, 2º nó, swtpm, imagem com guest-agent. | Só depois de montar a infra |
| **D — Princípio** | 23 | `not-implemented` por desenho/segurança, ou `partial` por proveniência honesta. | **Não — devem ficar** |

### Matemática do tecto

```
  270  células aplicáveis
− 171  já provadas
───────
   99  por provar (≈96 classificadas aqui)

Tecto honesto  = 270 − 23 (bucket D)            = 247 / 270   (91.5 %)   ← máximo teórico
Alcançável aqui = 171 + 9 (A) + ~9 (B viável aqui) ≈ 189 / 270 (~70 %)   ← sem infra nova
Falta para o tecto: 53 (C) + 2 (B pesado) = infra (PVE/OPNsense/2º nó/root) + engenharia
```

Mesmo montando **toda** a infra (um cluster PVE multi-nó, um OPNsense vivo, um 2º nó delonix, um dispositivo PCI sobrante com root, o swtpm provisionado, uma imagem guest com qemu-guest-agent) e fazendo **toda** a engenharia, param-se nas 23 do bucket D. O número honesto a perseguir é **247**, não 270 — e o caminho mais barato até perto dele é A agora, depois a infra de Proxmox/OPNsense (que sozinha vale 49 células).

## Recomendação

1. **Já (sem infra):** fechar o bucket A (9 células) — baterias live em libvirt (`vm.destroy`, `vm.network.bridge`, `vm.network.static-ip`, `vm.ip.observed`, `vm.restart-policy.native`, `vm.cpu.pinning`) e provas positivas de transporte (`access.transport-verified` CH/linux, `access.credential-in-vault` linux). Padrão idêntico ao `vm.migration.cold`: uma secção `e2e.sh` + `e2e:<título>` na arm. → leva a ~180/270.
2. **A seguir (CORRIGIDO 2026-10-27, medido ao vivo):** configurar o nó Proxmox `pve` NÃO é o lote fácil que este relatório estimou. FEITO: appliance `pve92` arrancado (nó `pve` @ 192.168.30.117), provider configurado (AVAILABLE=yes), e a suite `live.rs` validada ponta-a-ponta (`cria_arranca_e_destroi_contra_um_no_real` PASSA contra o PVE, 47s). ACHADO: os **40 supported já estavam provados**; configurar o nó re-confirma-os mas **desbloqueia ~0 células novas sozinho**. Os 12 partial / 9 not-impl de compute e 7/9 de rede precisam, célula a célula, de: **medição no dataplane/guest** (firewall «read back live, not measured on a packet»; hotplug cpu/mem «mock-tested, never run against a guest» → precisa de um guest com qemu-guest-agent), **engenharia** (`async-operations` sem job handle; `vm.template` sem verbo de engine; `storage.pools` sem listagem; wire de vlan/vxlan/snat/ipv6/net.observe), ou **multi-nó** (migração/HA). `provider.availability` e `access.*` são proveniência honesta (bucket D). Conclusão: Proxmox é uma campanha B+C real, não flips; o OPNsense idem (precisa do appliance vivo + dataplane). A estimativa «~230/270» era otimista — retirada.
3. **Engenharia (bucket B):** exportadores de métricas per-VM/per-workload + `host.capacity`/`host.health`, `net.observe` com comparação ao registo, `net.verify.dataplane`, selectores de label do `firewall.workload-peer`.
4. **Nunca:** bucket D. Mudá-las esvazia o significado da palavra `supported`.

---

## Anexo — classificação célula-a-célula

### Bucket A — Só-falta-prova (alcançável NESTE host, sem feature nova) — 9 células

| Provider | Capability | Estado | O que a graduação exigiria |
|---|---|---|---|
| cloud-hypervisor | `access.transport-verified` | partial | o transporte É verificado (socket unix/SO_PEERCRED); falta bateria positiva |
| libvirt | `vm.cpu.pinning` | partial | funciona em qemu:///system (presente aqui); só cai em session — falta bateria live |
| libvirt | `vm.destroy` | partial | funciona em libvirt aqui; falta só a bateria live citada |
| libvirt | `vm.ip.observed` | partial | funciona em libvirt aqui; falta só a bateria live citada |
| libvirt | `vm.network.bridge` | partial | funciona em libvirt aqui; falta só a bateria live citada |
| libvirt | `vm.network.static-ip` | partial | funciona em libvirt aqui; falta só a bateria live citada |
| libvirt | `vm.restart-policy.native` | partial | funciona em libvirt aqui; falta só a bateria live citada |
| linux | `access.credential-in-vault` | partial | funciona (resolvido do vault, nunca persistido); falta bateria citada |
| linux | `access.transport-verified` | partial | o transporte É verificado (socket unix/SO_PEERCRED); falta bateria positiva |

### Bucket B — Engenharia (código novo para completar a feature) — 11 células

| Provider | Capability | Estado | O que a graduação exigiria |
|---|---|---|---|
| cloud-hypervisor | `host.capacity` | not-implemented | exportar as métricas em falta / construir o relatório de capacidade |
| cloud-hypervisor | `metrics.network.per-workload` | partial | exportar as métricas em falta / construir o relatório de capacidade |
| cloud-hypervisor | `metrics.prometheus` | partial | exportar as métricas em falta / construir o relatório de capacidade |
| libvirt | `host.capacity` | not-implemented | exportar as métricas em falta / construir o relatório de capacidade |
| libvirt | `metrics.network.per-workload` | not-implemented | exportar as métricas em falta / construir o relatório de capacidade |
| libvirt | `metrics.prometheus` | partial | exportar as métricas em falta / construir o relatório de capacidade |
| libvirt | `vm.snapshot.memory` | partial | recuperação do CONTEÚDO de memória in-guest não é observada |
| linux | `firewall.workload-peer` | partial | selectores de label (ADR-0024) por implementar |
| linux | `net.ipam.dhcp` | partial | o endereço do tap CH fica fora do livro de IPAM (auditoria 62 §3) |
| linux | `net.observe` | partial | read-back existe; falta comparar com o registo (ADR-0059 F4) |
| linux | `net.verify.dataplane` | not-implemented | construir uma probe ativa de dataplane + asserção |

### Bucket C — Infra (root / dispositivo / 2º nó / PVE vivo / OPNsense) — 53 células

| Provider | Capability | Estado | O que a graduação exigiria |
|---|---|---|---|
| cloud-hypervisor | `vm.device.pci-passthrough` | partial | root + dispositivo PCI sobrante num grupo IOMMU isolado |
| libvirt | `vm.backup.quiesced` | partial | imagem de guest offline com qemu-guest-agent |
| libvirt | `vm.device.pci-passthrough` | partial | root + dispositivo PCI sobrante num grupo IOMMU isolado |
| libvirt | `vm.device.tpm` | partial | swtpm a correr como a identidade do qemu + permissão AppArmor (provisão do host) |
| linux | `container.gpu.cdi` | partial | run live precisa de um driver NVIDIA carregado |
| linux | `net.overlay.encrypted` | partial | encaminhamento inter-nó precisa de um 2º nó |
| linux | `net.overlay.vxlan` | partial | encaminhamento inter-nó precisa de um 2º nó |
| linux | `net.vlan` | partial | só root (802.1Q numa NIC física) |
| opnsense | `firewall.icmp-type` | not-implemented | precisa de um appliance OPNsense vivo para conduzir e ler de volta |
| opnsense | `net.apply.rollback` | not-implemented | precisa de um appliance OPNsense vivo para conduzir e ler de volta |
| opnsense | `net.dns.authoritative` | not-implemented | precisa de um appliance OPNsense vivo para conduzir e ler de volta |
| opnsense | `net.dns.records` | not-implemented | precisa de um appliance OPNsense vivo para conduzir e ler de volta |
| opnsense | `net.gateway.multi-wan` | not-implemented | precisa de um appliance OPNsense vivo para conduzir e ler de volta |
| opnsense | `net.gateway.update-in-place` | not-implemented | precisa de um appliance OPNsense vivo para conduzir e ler de volta |
| opnsense | `net.gateway.vpn` | not-implemented | precisa de um appliance OPNsense vivo para conduzir e ler de volta |
| opnsense | `net.ipam.dhcp` | not-implemented | precisa de um appliance OPNsense vivo para conduzir e ler de volta |
| opnsense | `net.ipam.reservation` | not-implemented | precisa de um appliance OPNsense vivo para conduzir e ler de volta |
| opnsense | `net.nat.npt` | not-implemented | precisa de um appliance OPNsense vivo para conduzir e ler de volta |
| opnsense | `net.nat.one-to-one` | not-implemented | precisa de um appliance OPNsense vivo para conduzir e ler de volta |
| opnsense | `net.verify.dataplane` | not-implemented | precisa de um appliance OPNsense vivo para conduzir e ler de volta |
| proxmox | `access.credential-in-vault` | partial | nó Proxmox NotConfigured — precisa de PVE vivo (várias precisam de cluster multi-nó) |
| proxmox | `access.transport-verified` | partial | nó Proxmox NotConfigured — precisa de PVE vivo (várias precisam de cluster multi-nó) |
| proxmox | `firewall.default-deny` | partial | nó Proxmox NotConfigured — precisa de PVE vivo (várias precisam de cluster multi-nó) |
| proxmox | `firewall.egress-policy` | partial | nó Proxmox NotConfigured — precisa de PVE vivo (várias precisam de cluster multi-nó) |
| proxmox | `firewall.icmp-type` | not-implemented | nó Proxmox NotConfigured — precisa de PVE vivo (várias precisam de cluster multi-nó) |
| proxmox | `firewall.logging` | not-implemented | nó Proxmox NotConfigured — precisa de PVE vivo (várias precisam de cluster multi-nó) |
| proxmox | `firewall.per-workload` | partial | nó Proxmox NotConfigured — precisa de PVE vivo (várias precisam de cluster multi-nó) |
| proxmox | `firewall.source-filtering` | partial | nó Proxmox NotConfigured — precisa de PVE vivo (várias precisam de cluster multi-nó) |
| proxmox | `host.health` | not-implemented | nó Proxmox NotConfigured — precisa de PVE vivo (várias precisam de cluster multi-nó) |
| proxmox | `metrics.network.per-workload` | not-implemented | nó Proxmox NotConfigured — precisa de PVE vivo (várias precisam de cluster multi-nó) |
| proxmox | `metrics.prometheus` | not-implemented | nó Proxmox NotConfigured — precisa de PVE vivo (várias precisam de cluster multi-nó) |
| proxmox | `net.bridge` | partial | nó Proxmox NotConfigured — precisa de PVE vivo (várias precisam de cluster multi-nó) |
| proxmox | `net.ipv6` | not-implemented | nó Proxmox NotConfigured — precisa de PVE vivo (várias precisam de cluster multi-nó) |
| proxmox | `net.nat.snat` | not-implemented | nó Proxmox NotConfigured — precisa de PVE vivo (várias precisam de cluster multi-nó) |
| proxmox | `net.observe` | partial | nó Proxmox NotConfigured — precisa de PVE vivo (várias precisam de cluster multi-nó) |
| proxmox | `net.overlay.vxlan` | not-implemented | nó Proxmox NotConfigured — precisa de PVE vivo (várias precisam de cluster multi-nó) |
| proxmox | `net.rate-limit` | not-implemented | nó Proxmox NotConfigured — precisa de PVE vivo (várias precisam de cluster multi-nó) |
| proxmox | `net.routes` | not-implemented | nó Proxmox NotConfigured — precisa de PVE vivo (várias precisam de cluster multi-nó) |
| proxmox | `net.segment.remote` | partial | nó Proxmox NotConfigured — precisa de PVE vivo (várias precisam de cluster multi-nó) |
| proxmox | `net.verify.dataplane` | not-implemented | nó Proxmox NotConfigured — precisa de PVE vivo (várias precisam de cluster multi-nó) |
| proxmox | `net.vlan` | not-implemented | nó Proxmox NotConfigured — precisa de PVE vivo (várias precisam de cluster multi-nó) |
| proxmox | `provider.async-operations` | partial | nó Proxmox NotConfigured — precisa de PVE vivo (várias precisam de cluster multi-nó) |
| proxmox | `storage.pools` | partial | nó Proxmox NotConfigured — precisa de PVE vivo (várias precisam de cluster multi-nó) |
| proxmox | `system-container.backup` | partial | nó Proxmox NotConfigured — precisa de PVE vivo (várias precisam de cluster multi-nó) |
| proxmox | `system-container.firewall` | partial | nó Proxmox NotConfigured — precisa de PVE vivo (várias precisam de cluster multi-nó) |
| proxmox | `system-container.network.bridge` | partial | nó Proxmox NotConfigured — precisa de PVE vivo (várias precisam de cluster multi-nó) |
| proxmox | `system-container.oci-image` | partial | nó Proxmox NotConfigured — precisa de PVE vivo (várias precisam de cluster multi-nó) |
| proxmox | `vm.hotplug.cpu.add` | partial | nó Proxmox NotConfigured — precisa de PVE vivo (várias precisam de cluster multi-nó) |
| proxmox | `vm.hotplug.disk.remove` | not-implemented | nó Proxmox NotConfigured — precisa de PVE vivo (várias precisam de cluster multi-nó) |
| proxmox | `vm.hotplug.memory.add` | partial | nó Proxmox NotConfigured — precisa de PVE vivo (várias precisam de cluster multi-nó) |
| proxmox | `vm.hotplug.nic.remove` | not-implemented | nó Proxmox NotConfigured — precisa de PVE vivo (várias precisam de cluster multi-nó) |
| proxmox | `vm.network.static-ip` | not-implemented | nó Proxmox NotConfigured — precisa de PVE vivo (várias precisam de cluster multi-nó) |
| proxmox | `vm.template` | partial | nó Proxmox NotConfigured — precisa de PVE vivo (várias precisam de cluster multi-nó) |

### Bucket D — Princípio: DEVEM ficar não-provadas (desenho, segurança, proveniência) — 23 células

| Provider | Capability | Estado | O que a graduação exigiria |
|---|---|---|---|
| cloud-hypervisor | `provider.async-operations` | not-implemented | backend síncrono; nada assíncrono a reportar |
| cloud-hypervisor | `provider.availability` | partial | proveniência honesta: available() é barato; a sonda funda é o provider ls |
| cloud-hypervisor | `provider.events` | not-implemented | este backend não tem stream de eventos no contrato |
| cloud-hypervisor | `storage.pools` | not-implemented | é um backend de VM, não um gestor de pools de armazenamento |
| cloud-hypervisor | `vm.device.tpm` | not-implemented | o CH não tem dispositivo TPM |
| cloud-hypervisor | `vm.hotplug.cpu.remove` | not-implemented | não-ligada de propósito (inseguro forçar; vai por resize a frio) |
| cloud-hypervisor | `vm.hotplug.memory.remove` | not-implemented | não-ligada de propósito (inseguro forçar; vai por resize a frio) |
| cloud-hypervisor | `vm.ip.observed` | partial | proveniência honesta: o IP é COMPUTADO do MAC, nunca observado |
| libvirt | `access.transport-verified` | partial | honesto: só URIs locais; um URI libvirt remoto nunca é construído |
| libvirt | `provider.async-operations` | not-implemented | backend síncrono; nada assíncrono a reportar |
| libvirt | `provider.availability` | partial | proveniência honesta: available() é barato; a sonda funda é o provider ls |
| libvirt | `provider.events` | not-implemented | este backend não tem stream de eventos no contrato |
| libvirt | `storage.pools` | not-implemented | é um backend de VM, não um gestor de pools de armazenamento |
| libvirt | `vm.hotplug.cpu.remove` | not-implemented | não-ligada de propósito (inseguro forçar; vai por resize a frio) |
| libvirt | `vm.hotplug.memory.remove` | not-implemented | não-ligada de propósito (inseguro forçar; vai por resize a frio) |
| libvirt | `vm.migration.live` | not-implemented | fora de âmbito (migração viva NBD exige convergência que o motor não tem) |
| libvirt | `vm.raw-definition` | partial | escape hatch local-only de propósito; nunca alcançável pelo contrato do nó |
| linux | `provider.async-operations` | not-implemented | backend síncrono; nada assíncrono a reportar |
| linux | `provider.availability` | partial | proveniência honesta: available() é barato; a sonda funda é o provider ls |
| proxmox | `provider.availability` | partial | proveniência honesta: available() é barato; a sonda funda é o provider ls |
| proxmox | `provider.events` | not-implemented | este backend não tem stream de eventos no contrato |
| proxmox | `vm.hotplug.cpu.remove` | not-implemented | não-ligada de propósito (inseguro forçar; vai por resize a frio) |
| proxmox | `vm.hotplug.memory.remove` | not-implemented | não-ligada de propósito (inseguro forçar; vai por resize a frio) |

