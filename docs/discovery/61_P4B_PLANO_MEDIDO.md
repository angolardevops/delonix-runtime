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

## Adenda P4b.3 (2026-09-29) — o desenho, medido antes do código (proposta)

**Base:** `origin/main` `0f851aab`. O plano acima disse que a P4b.3 «leva um addendum próprio
antes do código». É este. Nada do que se segue foi escrito em Rust ainda.

### O que se mediu

A orquestração ocupa hoje as linhas **3905–5361** do `crates/adapters/delonix-vm/src/lib.rs`
(1 457 linhas; o plano media 4225–5417 sobre `355aea8a` — cresceu com o `resize`, o
`set_cloud_init`, o `move_to_node` e o `guest_info` do ADR-0053). As funções públicas são as
que o `-bin`, o `delonix-linux`, a `mgmt` e o MCP chamam: `create`/`create_with`,
`remove`/`remove_force`/`destroy`/`destroy_with`, `stop`, `pause`/`unpause`, `set_cloud_init`,
`resize`, `move_to_node`, `guest_info`, `snapshot`/`restore`/`snapshots`/`delete_snapshot`,
`backup_disk_live`, `apply_firewall`/`read_firewall`, `start`/`restart`/`status`/`list`.

Os nós externos desse intervalo (`grep` sobre o intervalo, e as chamadas contadas sem comentários nem mensagens):

| Nó | Sítios | Onde pertence |
|---|---|---|
| `store(base)` / `JsonStore` (`load`/`save`/`update`/`remove`/`list`) | 15 + 18 | o `StateRepository<Vm>` do D6, injectado |
| `backend_for` / `select_backend_requiring` / `require_capabilities` / `resolve_required_capabilities` / `firmware_boot_preference` / `standing_backend_choice` | 18 + 6 | o registo — que **ficou no `delonix-vm`** na P4b.2 (ver a adenda anterior) |
| `qemu-img` (overlay `create -b`, `info`) | 3 chamadas | um porto de disco local |
| `cloudinit::generate_seed_iso` → `cloud-localds` | 1 | um porto de seed |
| `virsh` (`domblklist`, `blockcommit`) + `libvirt_domain_uri`/`libvirt_cleanup`/`libvirt_poweroff` | 4 chamadas + 5 | **o backend libvirt**, não a orquestração |
| `network()` (o `static` do `set_network`) | 1 | o porto `VmNetwork`, que já está no compute — passado, não global |
| `vm_admission_check` → `/proc/meminfo` | 1 | uma pergunta ao host: `delonix-node` |
| `std::fs` sobre o directório da própria VM (criar, apagar ficheiros da VM, `path_size`, o lock) | 13 | fica no use case: é o root de estado do próprio contexto, o que o `delonix-node` já faz hoje |

E um facto sobre os contextos: **nenhum crate em `crates/contexts/` chama `Command::new`
hoje** (`grep`: zero). O `qemu-img` e o `cloud-localds` não podem simplesmente descer com a
orquestração.

### Quatro achados que mudam a frase do plano

1. **O `backup_disk_live` não é orquestração** (4880–5025, com o `blockcommit_argv`): é todo
   `virsh domblklist` + `qemu-img` + `virsh blockcommit --pivot`, ou seja o mecanismo do
   backend libvirt. Sobe um método ao porto — `VmBackend::backup_disk_live(vmdir, vm, dest,
   quiesce)`, por omissão a recusa `unsupported_*` que os outros verbos já usam — e o use case
   fica com `load` + despacho. Tal como o plano estava, este bloco iria parar ao contexto com
   `virsh` dentro.
2. **O «domínio libvirt sem registo» vive na orquestração** — `remove_inner` (limpa um domínio
   órfão de um `rm` antigo) e `stop` (desliga-o em vez de responder «no such VM»). É
   conhecimento do libvirt. Passa a uma pergunta ao conjunto de backends por nome, sem registo:
   `stop_unrecorded(name) -> Result<bool>` e `remove_unrecorded(name) -> Result<bool>`, que só o
   libvirt responde com `true`.
3. **O registo continua no adapter** (o desvio da P4b.2), por isso um use case no compute não
   pode chamar `backend_for`. A resolução do backend também é injectada — um porto `VmBackends`
   que o `delonix-vm` implementa sobre o seu registo até à P4b.4, e que a composição passa a
   implementar quando o registo descer.
4. **O `StateRepository<T>` não é object-safe** (o `update<F>` é genérico). Os use cases são
   genéricos sobre os portos, exactamente como o `resolve_run<I, S, D, H>` do `container run`
   (`docs/discovery/54`), e não recebem `&dyn`.

### Os portos (no `delonix_compute::ports`, ao lado do `VmNetwork`)

```rust
pub trait VmBackends {
    fn for_vm(&self, vm: &Vm) -> Result<Box<dyn VmBackend>>;
    /// A escolha do `create`: o nome pedido, o que o operador fixou, a auto-detecção
    /// por capacidades e a preferência de firmware — a política inteira do registo.
    fn select(&self, root: &Path, cfg: &VmConfig) -> Result<Box<dyn VmBackend>>;
    fn manages_own_storage(&self, want: Option<&str>) -> bool;
    fn stop_unrecorded(&self, name: &str) -> Result<bool>;
    fn remove_unrecorded(&self, name: &str) -> Result<bool>;
}
pub trait LocalDiskImages {
    /// O overlay qcow2 de uma VM sobre o disco base: `canonicalize`, formato do
    /// backing lido do ficheiro (nunca da extensão), tamanho pedido ≥ o do base.
    fn overlay(&self, vmdir: &Path, name: &str, base: &Path, size_gib: Option<u32>) -> Result<PathBuf>;
}
pub trait SeedBuilder {
    fn seed(&self, vmdir: &Path, name: &str, intent: &CloudInitIntent) -> Result<PathBuf>;
}
```

`LocalDiskImages` e `SeedBuilder` são dois portos e não um: o seed é cloud-init, não uma
imagem de disco, e um provider com storage própria (o Proxmox, `manages_own_storage`) não usa
nenhum dos dois. Na P4b.3 os três são implementados pelo próprio `delonix-vm` (é um adapter;
o `Command` é legítimo lá). Na P4b.4 a implementação do disco e do seed muda para um adapter
próprio — o `delonix-guestfs` que a ADR-0040 D2.3 já nomeia para o `qemu-img` —, e os dois
providers locais recebem-na da composição: um provider não pode depender de um adapter, e
duplicar o `qemu-img create` nos dois crates é a segunda cópia que o `mem_mib` já custou.

A admissão de RAM parte-se em dois: a leitura do `/proc/meminfo` desce para o `delonix-node`
(ao lado do `proc_starttime`, que o `adopt_pid_starttime` já usa), e o veredicto
(`admission_verdict`, puro e já testado) vai com o use case.

### O que não muda

- **A superfície pública do `delonix-vm`**: os 46 símbolos do `-bin` e os quatro do
  `delonix-linux`/`mgmt`/MCP continuam a resolver, agora como invólucros finos que abrem o
  `JsonStore`, passam o registo, o disco, o seed e a rede, e chamam o use case.
- **O formato em disco** de `Vm`/`VmBootSpec` e o ficheiro de lock por VM.
- A excepção `("dep", "delonix-vm", "delonix-state")` fica até à P4b.4 (os invólucros ainda
  abrem o `JsonStore`), mas os use cases deixam de depender do `delonix-state`.

### Em duas fatias, cada uma com o seu portão

- **P4b.3a — o conhecimento do backend sai da orquestração, ainda dentro do `delonix-vm`.**
  `VmBackend::backup_disk_live` (o libvirt implementa, os outros recusam por nome), o
  `stop_unrecorded`/`remove_unrecorded`, a leitura do `/proc/meminfo` no `delonix-node`, e os
  portos declarados no compute com a implementação no `delonix-vm`. Nenhuma função muda de
  crate. **Portão:** os testes do `delonix-vm` inalterados, a secção `vm` do `scripts/e2e.sh`
  (o `backup create vm` de uma VM libvirt a correr com dois discos, o caso que a bateria de
  2026-09-24 já cobre), e zero `virsh` no intervalo da orquestração.
- **P4b.3b — os use cases descem para `delonix_compute::vm`**, genéricos sobre
  `StateRepository<Vm>`/`VmBackends`/`LocalDiskImages`/`SeedBuilder`/`VmNetwork`, com testes no
  compute contra portos falsos (um repositório em memória, um backend que regista chamadas) —
  o ganho que justifica o corte: hoje estes caminhos só se testam contra um root de estado
  real. **Portão:** os 46 símbolos, a secção `vm` da bateria e o cenário de caos
  `control_restart` sem regressão, e um contador novo no `arch_fitness.py` —
  `context_spawns` (`Command::new` em `crates/contexts/`) com linha de base **0**, para o
  contexto nunca ganhar o primeiro `Command` pelo caminho.

### Provado vs não validado

Provado, por `grep`/`sed` sobre `0f851aab`: o intervalo, os nós e as contagens da tabela; que
o `backup_disk_live` e o tratamento do domínio órfão só chamam `virsh`/`libvirt_*`; que nenhum
contexto chama `Command::new`; que o `delonix-compute` já depende do `delonix-node`; que o
`StateRepository<T>` não é object-safe. Não validado: se o `select` cabe numa assinatura sem
arrastar mais política do que a listada (a auto-detecção e o firmware medem-se ao cortar), e o
custo real da P4b.3b — o entrançado só se mede ao desfazê-lo, como o plano já dizia.

## Adenda P4b.3a (2026-09-29) — o conhecimento de backend saiu da orquestração

**Feito, tudo ainda dentro do `delonix-vm`.** A orquestração passou de 1 457 para 1 165 linhas
e tem hoje **zero** `virsh`, `libvirt_domain_uri`/`libvirt_cleanup`/`libvirt_poweroff`,
`qemu-img`, `cloudinit::` e `backend_for` — contados sobre o intervalo `valid_vm_name` →
primeiro módulo de testes. Ficam as 15 chamadas a `store(base)`, que são da P4b.3b.

- **`VmBackend::backup_disk_live`** no porto (compute), com a recusa por omissão que a
  orquestração dava antes, com o mesmo texto e o mesmo DX-1514; o libvirt implementa-o com o
  corpo de antes (`libvirt_backup_disk_live`, agora ao lado do `LibvirtBackend`).
- **Os três portos** no `delonix_compute::ports` e a implementação no `delonix-vm`
  (`local_ports.rs`: `RegistryBackends`, `QemuImgDisks`, `CloudLocaldsSeed`). A escolha do
  backend de uma VM nova saiu do `create_with` para o `select_for_create`, junto do registo; o
  `prepare_local_overlay` passou para dentro do `QemuImgDisks`.
- **A leitura do `/proc/meminfo`** desceu para o `delonix-node` (`mem_available_mib`), e o
  veredicto de admissão ficou com a orquestração.
- **Um `eprintln!` a menos** numa biblioteca: o aviso de «cloud image em Cloud Hypervisor sem
  libvirt» passou a `tracing::warn!` (ratchet `library_prints` 90 → 89).

**Desvios ao esboço da adenda anterior, medidos ao cortar:**

- O `VmBackends` precisou de mais duas perguntas ao registo: `require` (as capacidades exigidas,
  que um `start` também verifica) e `declares` (a política de restart, o namespace e o
  anti-spoof perguntam o relatório de um backend). O «domínio sem registo» ficou em três
  métodos em vez de dois: `unrecorded(name) -> Option<&'static str>` diz QUE backend o tem (o
  `rm` anuncia-o no progresso), e `stop_unrecorded`/`remove_unrecorded` agem.
- O `LocalDiskImages::overlay` recebe o callback de progresso (o `CreateStage::Disk` só sai
  quando o overlay é mesmo criado) e devolve `(base, overlay)`, porque o registo da VM guarda os
  dois. O `SeedBuilder::seed` recebe o `VmConfig` inteiro: o seed lê o hostname, o utilizador,
  as chaves e os volumes.

**Uma correcção à adenda anterior.** Ela dizia que o portão desta fatia era «o `backup create
vm` de uma VM libvirt a correr com dois discos, o caso que a bateria de 2026-09-24 já cobre».
Não cobria: a linha `vm.backup-disk` do catálogo está `partial` precisamente porque a bateria
só fazia o backup de um container. Esta fatia acrescenta o caso à secção de snapshots libvirt
do `scripts/e2e.sh`: com a VM a correr, o arquivo é criado, a VM continua `running`, e o
`domblklist` mostra-a a escrever no seu próprio overlay (nem no temporário, nem na imagem base
— o defeito que um `blockcommit` sem `--top`/`--base` esconde atrás de «Successfully
pivoted»). **Medido:** as secções de VM da bateria contra o binário desta árvore deram 101
PASS, 0 FAIL, com os quatro checks novos entre eles. A linha do catálogo não foi promovida:
isso é decisão da ADR-0050, com um check de restauro ao lado.

## Adenda P4b.3b (2026-09-29) — os use cases desceram para o contexto

**Feito.** A orquestração saiu do `delonix-vm` para `delonix_compute::vm` (1 612 linhas com os
testes novos): métodos de um `VmEngine<'a, R, B, D, S>` que recebe o root, o
`StateRepository<Vm>`, os três portos e a rede, genérico sobre eles como o `resolve_run` do
`container run`. Desceram com ela as funções puras que ela usava (`valid_vm_name`,
`vm_namespace_of`, a admissão de RAM, `resolve_required_capabilities`,
`adopt_pid_starttime`/`argv_is_vmm_for`, `boot_spec_of`/`config_from`) e o `Destroyed`. O
`delonix-vm` passou de 9 249 para 8 331 linhas: monta o engine por chamada (`engine(base)`: o
`JsonStore`, o `RegistryBackends`, o `QemuImgDisks`, o `CloudLocaldsSeed` e a rede registada) e
mantém as 24 funções públicas como invólucros de uma linha. Nenhum chamador do `-bin`, do
`delonix-linux`, da `mgmt` ou do MCP mudou; os testes do `delonix-vm` continuam a chegar às
funções puras pelos nomes de sempre (importadas de volta só para testes).

- **O que a P4b.3a deixou passar:** o `check_allow_mac_spoofing` é conhecimento do libvirt (o
  nome do backend e o filtro anti-spoof) e o `create_with` chamava-o. Passou a
  `VmBackends::admit(backend_id, cfg)`, implementado no `delonix-vm` pela função de sempre —
  mesmo texto, mesmo código.
- **O `into_root()` sobre o erro do store** deu lugar a `e.is_not_found()` (ADR-0043 D4): o erro
  que chega pelo porto é o partilhado, e é a classe que se pergunta, não a variante.
- **O contexto não corre programas:** o `arch_fitness.py` ganhou o contador `context_spawns`
  (`Command::new` em `crates/contexts/`), com linha de base **0**. Verificado: um `Command::new`
  temporário no compute faz o portão chumbar com «new debt entered».
- **Testes contra portos falsos** (`vm::tests`, 6): um repositório em memória, um backend que
  regista chamadas, disco e seed falsos — overlay e seed numa VM local e nenhum dos dois num
  backend com storage própria, um nome inválido que não chega a porto nenhum, o `stop` de um
  nome sem registo a perguntar aos backends (4501 quando nenhum o tem), o `status` a reconciliar
  uma VM que o backend parou, e o `stop` a registar `Stopped`. Retirar a guarda
  `!own_storage` do seed faz chumbar o teste respectivo (verificado).

**Fica para a P4b.4:** a excepção `("dep", "delonix-vm", "delonix-state")` (o `engine(base)`
ainda abre o `JsonStore`), o registo, e os dois backends locais, que saem para os seus crates.

## Adenda P4b.4 (2026-09-29) — o desenho, medido antes do código (proposta)

**Base:** `origin/main` depois do #597. Como a P4b.3, esta fatia leva um desenho antes do código,
porque a medição encontrou um ponto que o plano não confrontou: **onde se monta o engine quando
o `delonix-vm` sair.**

### O que se mediu

O `delonix-vm` depois da P4b.3b: 8 329 linhas no `lib.rs` (1–1150 helpers e registo; 1151–1952
Cloud Hypervisor; 1953–4038 libvirt; 4039–4405 a montagem do engine e os 24 invólucros;
4406–8329, **3 924 linhas de testes**), mais `capabilities.rs` (749: as duas declarações ADR-0050
e as sondas `LibvirtHost`/`CloudHypervisorHost`), `cloudinit.rs` (317), `local_ports.rs` (157) e
`provider.rs` (311, o `LocalVmProvider` da P4b.1).

**O que os dois backends partilham é pouco** (chamadas por zona, `grep`):

| Helper | CH | libvirt | Natureza |
|---|---|---|---|
| `stable_cmd` / `capture` / `binary_in_path` | 1 / 1 / 1 | 5 / 14 / 1 | correm programas |
| `mac_for` | 1 | 1 | pura |
| `network()` | 5 | 0 | o porto `VmNetwork` |
| `terminate_vmm`, `vmm_left`, `wait_vmm_left`, `vmm_to_signal`, `shq`, `memory_arg`, `cpus_arg` | sim | 0 | só CH |
| `is_rootless`, `run_quiet`, `disk_backing_format`, `pick_lease_ip`, `leases_max_expiry` | 0 | sim | só libvirt |

**Os testes**, classificados pelo que exercitam (114): libvirt 46, Cloud Hypervisor 27, registo 7,
invólucros públicos 4, outros 30.

**Quem depende do `delonix-vm`** (dependências reais nos `Cargo.toml`, não menções): o `-bin` e
três interfaces — `delonix-mgmt`, `delonix-mcp` e `delonix-node-api` —, e estas usam o engine
completo (`create`, `start`, `status`, `list`) e os relatórios de capacidades.

### O ponto que o plano não confrontou

O plano dizia «`delonix-vm` fora do workspace» no fim da P4b.4. Mas a **montagem** do engine — o
registo com os dois backends locais por uma ordem que decide a auto-detecção, o `JsonStore`, o
disco e o seed locais, e a rede registada — tem de viver num sítio que os quatro consumidores
partilhem, e a tabela `ALLOWED` não tem esse sítio:

- um **adapter** não depende de providers;
- uma **interface** não depende de outra interface;
- um **contexto** não conhece providers nem corre programas;
- montar em cada consumidor são **quatro cópias** do registo, e a ordem de registo é o que decide
  qual backend uma VM sem `--backend` recebe — quatro cópias divergem.

É o que a camada de aplicação da ADR-0040 (P5) existe para resolver, e ela ainda não existe.

### A proposta

1. **Dois crates de provider**: `delonix-provider-cloud-hypervisor` e `delonix-provider-libvirt`,
   cada um com o seu `VmBackend`, a sua declaração ADR-0050 e a sua sonda (o `capabilities.rs`
   parte-se em dois), os seus helpers e os seus testes; cada um expõe `registration()`, como o
   Proxmox já faz. O libvirt leva também o que a P4b.3 lhe deu por nome: o backup a quente, o
   domínio sem registo (`unrecorded`/`stop_unrecorded`/`remove_unrecorded`) e o `admit`.
2. **`mac_for`** (pura) desce para o compute. **`stable_cmd`/`capture`/`binary_in_path`** ficam uma
   cópia em cada provider (≈20 linhas), cada uma com o teste que fixa o `LC_ALL=C` — a regressão
   que estas funções existem para impedir (o `virsh` é gettext). Não há outro sítio legal: um
   provider não depende de outro provider nem de um adapter, e um contexto já não corre programas
   (`context_spawns` = 0). A cópia é pequena e o teste é o que impede as duas de divergirem na
   única coisa que importa.
3. **O registo desce para o compute** (`delonix_compute::vm_registry`, o que o ADR D4 já previa),
   vazio: ninguém o semeia por dentro; a montagem regista os backends locais por uma ordem
   explícita e escrita num sítio só.
4. **Um adapter `delonix-guestfs`** (o nome que a ADR-0040 D2.3 já reservou para o `qemu-img`)
   leva o `cloudinit.rs` e as implementações `LocalDiskImages`/`SeedBuilder`.
5. **O `delonix-vm` fica, reduzido à montagem, até à P5** — o engine por chamada, o registo dos
   dois locais, os 24 invólucros públicos e o `LocalVmProvider` —, com **duas excepções novas e
   declaradas** no `arch_fitness.py`, `("dep", "delonix-vm", "delonix-provider-libvirt")` e
   `("dep", "delonix-vm", "delonix-provider-cloud-hypervisor")`, fase **P5**, razão escrita: é a
   raiz de composição que a camada de aplicação vai absorver. A regra do portão («uma excepção tem
   de nomear a fase que a remove») é o que torna isto honesto: a dívida fica à vista, com data de
   saída, em vez de quatro cópias do registo espalhadas pelos consumidores.

   **Isto desvia-se do plano** («`delonix-vm` fora do workspace») e é por isso que vai por escrito
   antes do código. A alternativa que cumpre o plano à letra é acrescentar uma camada à tabela
   `ALLOWED` — decisão de estrutura (ADR-0040), não de uma fatia.

### Em fatias, cada uma com o seu portão

- **P4b.4a** — `mac_for` no compute, o registo no compute (semeado pela montagem), o adapter
  `delonix-guestfs` com o `cloudinit.rs` e os dois portos. Nenhum backend muda de crate.
- **P4b.4b** — o crate `delonix-provider-libvirt` (backend, declaração, sonda, helpers, testes).
- **P4b.4c** — o crate `delonix-provider-cloud-hypervisor`, idem.
- **P4b.4d** — o `delonix-vm` reduzido à montagem, as duas excepções declaradas, `LAYERS` com os
  dois nomes novos, e o manual regenerado.

**Portão de cada uma:** os símbolos públicos dos quatro consumidores a resolver sem mudar de
linha, a secção `vm` da bateria (101 checks hoje) e o caos `control_restart` sem regressão, o
`provider ls`/`provider matrix` byte-a-byte iguais (as declarações só mudam de crate), e o
`arch_fitness.py` verde com as excepções que cada fatia declara ou remove.

### Provado vs não validado

Provado, por `grep` sobre o código do #597: as zonas e as contagens, os helpers partilhados, a
repartição dos testes, e que só o `-bin` e as três interfaces dependem do `delonix-vm`. Não
validado: o custo da partição dos 3 924 linhas de testes (a classificação acima é por palavras,
não por leitura) e se o `LocalVmProvider` cabe na montagem sem arrastar mais nada.
