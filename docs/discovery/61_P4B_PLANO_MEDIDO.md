# 61 — P4b: o plano medido do corte de `delonix-vm` (ADR-0044, aceite a 2026-09-24)

**Data:** 2026-09-24 · **Base:** `origin/main` (`355aea8a`, v4.4.0 +3 commits) ·
**O que é:** não é um spike — é a medição que decide em que ordem o P4b se corta, feita
ANTES de mover uma linha, para que cada fatia tenha um portão próprio (ADR-0044 D9) e
nenhuma deixe o `arch_fitness.py` vermelho pelo caminho.

## A pergunta

O ADR-0044 D9 descreve o P4b numa linha: «`VmSpec`/`Extensions`/`VmProvider` land in
`delonix-compute`; `delonix-provider-cloud-hypervisor`/`-libvirt` split out of
`delonix-vm`, `delonix-vm` retired». O crate tem 8 561 linhas num só `lib.rs` e seis
consumidores. A pergunta é o que tem de sair PRIMEIRO para os dois crates de provider
poderem existir sem violar a direcção das camadas, e a resposta vem de uma regra que o
ADR cita mas não confronta com o código: **um crate `PROVIDER` só pode depender de
`FOUNDATION` e `CONTEXT`** (`scripts/arch_fitness.py`, `ALLOWED`). Um
`delonix-provider-libvirt` que dependesse de `delonix-vm` (um `ADAPTER`) chumba o portão
no primeiro commit. Logo tudo o que um backend implementa ou chama tem de estar num
contexto antes de o backend mudar de crate — e isso é mais do que o porto D1–D3.

## O que se mediu

**O crate por zonas** (`crates/adapters/delonix-vm/src/lib.rs`, números de linha de hoje):

| Zona | Linhas | O que é |
|---|---|---|
| helpers partilhados | 1–1602 | `VmConfig` (67), o porto `VmBackend` (849), o registo (`BackendRegistration`/`register_backend`/`select_backend*`/`require_capabilities`/`auto_detect`, 1066–1478), backend por omissão (1495–1560), `mem_mib`, `mac_for`, `vm_namespace_supported`, admissão de RAM, `terminate_vmm`, parsers de `qemu-img`/leases |
| Cloud Hypervisor | 1603–2378 | `CloudHypervisorBackend`: `boot_ch`, console/serial, snapshots offline |
| libvirt | 2379–4224 | `LibvirtBackend`: `libvirt_domain_xml`, `with_stopped_domain`, snapshots, reserva DHCP |
| orquestração | 4225–5417 | `create`/`create_with`, `remove`/`destroy_with`, `stop`/`pause`/`unpause`, snapshots, `backup_disk_live`, `start`/`restart`/`status`/`list` |
| testes | 5418–8561 | quatro módulos `#[cfg(test)]` |

Mais `capabilities.rs` (399, as duas declarações ADR-0050 com sonda do host),
`cloudinit.rs` (320), `error.rs` (465) e o `provider_spike.rs` (412), que esta fatia promove.

**Os consumidores**, por `grep -rho 'delonix_vm::…'`:

| Consumidor | Símbolos distintos | Os que pesam |
|---|---|---|
| `delonix-runtime-bin` | 46 | `list` (24 sítios), `status` (19), `create` (12), `valid_backend_name` (9), `stop`, `remove_force`, `VmConfig`, `create_with`, `backup_disk_live`, snapshots, `set_network` |
| `delonix-proxmox` | 5 | `BackendRegistration`, `register_backend`, `Error::Engine`, `cloudinit::DEFAULT_CI_USER`, `VmVolume` (já é do compute) |
| `delonix-linux`, `delonix-mgmt`, `delonix-mcp` | 4 | `create`, `list`, `start`, `status` |

**Os nós de dependência de cada zona** — o que a impede de viver num contexto tal como
está (`grep` sobre cada intervalo, não leitura):

| Zona | Nós externos | Onde cada um pertence |
|---|---|---|
| orquestração | `JsonStore::update` + `store(base)` (delonix-state); `std::fs::{remove_file, canonicalize, copy, create_dir_all, read_dir, remove_dir_all, symlink_metadata}` sobre o root de estado; `qemu-img create` (overlay) e `cloud-localds` (seed) por `Command` | o registo é o `StateRepository<Vm>` do D6 (o trait já está em `delonix_model::ports`, o `impl` para `JsonStore<T>` já existe); o `fs` sobre o próprio root é o que o `delonix-node` já faz hoje num contexto; o overlay e o seed são trabalho de PROVIDER LOCAL, não de use case |
| Cloud Hypervisor | `stable_cmd("qemu-img")`, `network()` (o porto `VmNetwork`, já no compute) | fica no provider; o `VmNetwork` já é porto |
| libvirt | `virsh` (15 sítios), `delonix_state::write_atomic` (3: o XML do domínio e os snapshots preservados), `libc::getuid/getgid` (seclabel rootless) | `virsh` fica no provider; o `write_atomic` é o `ConfigWriter` que o addendum D6 de 2026-09-19 já mapeou para este crate; `libc` é dependência de workspace, legítima num provider |
| helpers | `terminate_vmm`/`proc_state`/`vmm_left` (sinais e `/proc`, via `delonix_node::safe_to_signal`), admissão de RAM (`/proc/meminfo`) | os dois locais partilham-nos: ou `delonix-node` (perguntas ao host, onde `safe_to_signal` já vive) ou o contexto |

Dois factos que só a medição deu e o ADR não tem: (1) **a orquestração não é dos
providers** — `create_with` faz `canonicalize` do disco, cria o overlay, gera o seed, pede
o `tap` ao holder e só depois chama `backend.boot`; nada disso é do backend, e é por isso
que o `manages_own_storage` existe (ADR-0008): a orquestração local está entrançada com
os dois backends locais **dentro do mesmo ficheiro**, e o corte tem de a separar antes
de os separar; (2) **`delonix-proxmox` pode ficar dep-limpo ANTES do P4c** — dos cinco
símbolos que usa, só `BackendRegistration`/`register_backend` e `Error::Engine` são do
`delonix-vm`; quando o registo e o erro estiverem no contexto, a excepção
`("dep", "delonix-proxmox", "delonix-vm")` cai sem que o Proxmox mude para o porto novo.

## As fatias, pela ordem que a direcção das camadas obriga

**P4b.1 — o porto no contexto (este PR).** `delonix_compute::vm_provider`: `VmSpec`,
`Extensions` (+ `CloudHypervisorExt`/`LibvirtExt`), `ProviderId`, `Provider` (id +
`capabilities()` = o `ProviderReport` da ADR-0050, `health()` derivado dele),
`IpConfidence`, `VmHandle`, `VmObservation`, `VmProvider`. O `provider_spike.rs` do
`delonix-vm` passa a `provider.rs`: `spec_to_config` (a única coisa que conhece
`VmConfig`), `LocalVmProvider` e `registry(id)`, a implementar o porto do contexto. Uma
correcção ao spike, do D1 do próprio ADR: `bridge` é universal (o Proxmox lê-o) e o spike
tinha-o em `LibvirtExt`; passa para `VmSpec`. Nada muda para nenhum chamador. **Portão:**
testes de `delonix-compute` e `delonix-vm`, `provider_live` a compilar, fitness
inalterado (o contexto não ganha dependências), `cargo check --workspace`.

**P4b.2 — o que os providers implementam sai do adapter.** Para `delonix-compute`:
`VmConfig`, `Boot`, `CreateStage`/`DestroyStage`, o trait `VmBackend`, o registo inteiro
(`BackendRegistration`, `BackendFactory`, `ReportFactory`, `register_backend`,
`select_backend*`, `resolve_required_capabilities`, `require_capabilities`,
`auto_detect`, `provider_reports`), `mem_mib`, `valid_vm_name`, `mac_for`,
`vm_namespace_supported`/`restart_policy_unsupervised`, `cloudinit::DEFAULT_CI_USER`, e
as variantes de `Error` que o Proxmox constrói. O `delonix-vm` re-exporta tudo por
`pub use` — zero chamador muda de linha. **O que se decide aqui**: o registo é um
`static` com `OnceLock<RwLock<Vec<…>>>`; num contexto continua a ser um mapa povoado no
arranque pela composição (`cmd/vmbackends.rs`), sem I/O ao registar (ADR-0008). **Portão:**
a excepção `("dep", "delonix-proxmox", "delonix-vm")` removida — o Proxmox passa a
depender só de `delonix-compute` — e os 126 testes do `delonix-vm` inalterados.

**P4b.3 — a orquestração vira use cases do contexto.** `create_with`/`stop`/`start`/
`status`/`list`/`remove`/`destroy_with`/snapshots/`backup_disk_live` para
`delonix_compute` (um módulo `vm`, o `app` do D4), a receber `&impl StateRepository<Vm>`
da composição em vez de abrir o `JsonStore` (fecha metade da excepção
`("dep", "delonix-vm", "delonix-state")`). O overlay (`qemu-img`) e o seed
(`cloud-localds`) saem de `create_with` para um porto `LocalDiskImages`/`SeedBuilder`
que os dois providers locais usam e um adapter implementa — o `delonix-guestfs` que a
ADR-0040 D2.3 já nomeia para `qemu-img`/`virt-customize` —, porque um provider não pode
depender de um adapter e duplicar o `qemu-img create` nos dois crates é a segunda cópia
que este repo já pagou com o `mem_mib`. **É o ponto de desenho da série** e leva um
addendum próprio antes do código. **Portão:** os 46 símbolos do `-bin` continuam a
resolver (por `pub use`), a bateria `scripts/e2e.sh` secção `vm` e o cenário de caos
`control_restart` sem regressão, `VmNetwork` continua a ser o único caminho para o holder.

**P4b.4 — os dois crates de provider, e o `delonix-vm` retira-se.**
`crates/providers/delonix-provider-cloud-hypervisor` e `-libvirt`, cada um com o seu
`VmBackend`/`VmProvider`, a sua declaração de capacidades e a sua sonda (o
`capabilities.rs` parte-se em dois), a registar-se pela composição como o Proxmox já faz.
O que sobrar em `delonix-vm` é medido nessa altura; a intenção é zero. `LAYERS` ganha os
dois nomes, `dev_docs.py` regenera o manual, `install.sh` não muda (os providers são
bibliotecas ligadas ao `delonix`). **Portão:** `delonix-vm` fora do workspace, as
excepções P4 restantes deste crate fora da tabela, `provider_live` e a secção `vm` da
bateria verdes contra o binário construído dos crates novos.

## O que fica de fora, de propósito

O Proxmox no porto `VmProvider` (P4c), o `delonix-launcher` (P4d, é outro adapter),
`delonix-provider-mount`/`-truenas` (P4e), e um `DiskSource` tipado — cada um com a sua
linha no D9. E **nenhuma fatia muda o formato em disco de `Vm`/`VmBootSpec`**: o P4b move
código, não registos.

## Provado vs não validado

Provado: as zonas, os consumidores e os nós acima, por `grep`/`sed` sobre a base
indicada; que o `delonix-proxmox` só toca cinco símbolos; que a regra `PROVIDER →
{FOUNDATION, CONTEXT}` está no `ALLOWED` do fitness e não é opinião. Não validado:
o custo real das fatias 3 e 4 (o entrançado orquestração↔backends só se mede ao
desfazê-lo), e se `delonix-linux`/`-mgmt`/`-mcp` chegam aos use cases por `pub use` ou
precisam de mudar de importação — os quatro símbolos são os mesmos, por isso deve ser
`pub use`.

## Adenda P4b.2 (2026-09-26) — o que se moveu, e o registo que ficou para a P4b.4

**Feito.** Para `delonix-compute`: `vm_backend` (`VmConfig`, `CloudInitIntent`, `Boot`,
`CreateStage`/`DestroyStage`, o trait `VmBackend` com os três `unsupported_*`,
`BackendFactory`/`ReportFactory`/`BackendRegistration`, `mem_mib`/`parse_mem_mib`,
`DEFAULT_CI_USER`), `vm_error` (o `Error`/`Result` inteiros, com o `From<Error> for Dx`) e
`vm_firewall`. O `delonix-vm` re-exporta tudo pelos nomes de sempre; nenhum chamador do
`-bin`, do `delonix-linux`, da `mgmt` ou do MCP mudou uma linha. O `delonix-proxmox` passou a
depender só de `delonix-compute`, e a excepção `("dep", "delonix-proxmox", "delonix-vm")`
saiu do `arch_fitness.py` (10 → 9 excepções).

**O desvio ao plano, e porquê.** O plano punha o REGISTO (o `static`, `register_backend`,
`select_backend*`, `auto_detect`) nesta fatia. Medido ao cortar: o registo é semeado com
`CloudHypervisorBackend` e `LibvirtBackend`, que só saem do `delonix-vm` na P4b.4. Movê-lo
agora obrigava a um gancho de sementeira entre crates, ou a deixar a ordem da auto-detecção
depender de quem regista primeiro. O portão desta fatia não precisa dele: o Proxmox deixa de
registar-se sozinho e devolve um `BackendRegistration` (`delonix_proxmox::registration`), que
a raiz de composição (`cmd/vmbackends.rs`) regista — a forma que o ADR-0008 descreve. O
registo desce na P4b.4, quando os dois locais também forem registados pela composição e não
houver sementeira nenhuma para transportar.

**Duas conversões ficaram no adapter**, por regra de órfãos: `From<cloudinit::Error>` (o tipo
de origem é do `delonix-vm`, fica como `impl`) e `From<delonix_state::Error>` (os dois tipos
são agora de outros crates, logo passou a função `state_err`, chamada nos 14 sítios que
faziam `?` sobre o `JsonStore`).
