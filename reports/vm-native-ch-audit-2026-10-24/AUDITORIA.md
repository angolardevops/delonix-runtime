# Auditoria — caminho NATIVO de VMs (Cloud Hypervisor, sem libvirt)

**Data:** 2026-10-24 · **Host de referência:** ver `evidence/reference-config.txt`
**Binário auditado:** `delonix 5.1.1` (commit `f49d3570c`, +45 desde v5.1.1) ·
**`develop` do repo:** `69c349c3`.
**Veredicto curto:** o caminho nativo **funciona ponta-a-ponta e é reproduzível**
(harness: **28/28** asserções, `RC=0`, `evidence/harness-run-2026-10-24.txt`).
Não existe libvirt no caminho de execução nativo. As "falhas" iniciais que
persegui eram **topologia esperada do SDN rootless**, não defeitos. Há **um bug
de correcção real** (hint `ssh <ip>`) e uma **dívida de validação** (capabilities
honestamente marcadas `partial` que o harness agora prova).

---

## 1. Como o Delonix cria e controla uma VM nativa (componentes reais)

Demonstrado ao vivo: `vm create … --backend cloud-hypervisor` arranca este
processo (sem libvirt, sem qemu), confirmado por `ps`:

```
cloud-hypervisor --api-socket <root>/vms/<vm>.sock \
  --firmware /usr/local/share/delonix/CLOUDHV.fd \
  --disk path=<root>/vms/<vm>.qcow2,image_type=qcow2,backing_files=on \
  --disk path=<root>/vms/<vm>/seed.iso \
  --cpus boot=2 --memory size=2048M \
  --net tap=vt<hex>,mac=52:54:00:.. --serial socket=<root>/vms/<vm>.console --console off
```

| Responsabilidade | Componente | Âncora |
|---|---|---|
| Construção do argv / arranque | `boot_ch` | `providers/delonix-provider-cloud-hypervisor/src/lib.rs:1491-1592` |
| Boot: firmware **EDK2 `CLOUDHV.fd`** (não UEFI-libvirt) ou kernel directo | `default_ch_firmware`, `DEFAULT_CH_FIRMWARES` | CH `:1317-1352` |
| Controlo do VMM: **HTTP/1.1 à mão sobre o api-socket Unix** (sem `ch-remote`, sem parse de logs) | `ch_api_call_with_body` | CH `:1398-1451` |
| pause/resume/stop/resize | `PUT /api/v1/vm.{pause,resume}`, `vmm.shutdown`, `vm.resize` | CH `:973-979,918-932,1012-1018,1072-1078` |
| Readiness = **estado da VM**, não "processo vivo" | `wait_vmm_ready` → `GET /api/v1/vm.info` espera `"state":"Running"` | CH `:1725-1764` |
| Guarda pid+starttime (anti-reuso de PID) + zombie | `vmm_left`, `vmm_to_signal`, `safe_to_signal` | CH `:130-190,801-804` |
| TAP criado **dentro da netns de infra rootless** (`nsenter`) e ligado à bridge | `boot_on_network`→`do_vmtap` | CH `:709-749`; sdn `infra.rs:4417-4448` |
| Anti-spoof (tabela nft `dlxspoof`, binding do MAC/lease) | `do_vmtap` | sdn `infra.rs:4163,4431-4446` |
| Seed cloud-init NoCloud (user-data/meta-data/network-config → `cloud-localds`) | `generate_seed_iso` | adapters/delonix-vm `cloudinit.rs:145-219` |
| Persistência: record JSON, overlay qcow2, pid, log, sockets | `JsonStore`/`vms_dir` | delonix-vm `lib.rs:91-103`; compute `vm.rs:333-353` |

**Processos vivos durante a VM:** apenas o `cloud-hypervisor` (um por VM) +
o holder da netns (`delonix netns pin`) + `slirp4netns` partilhados pela SDN.
Sem daemon de VMs, sem libvirtd no caminho.

## 2. Contrato do backend e estado

Dois ports coexistem (migração ADR-0044 em curso):
- **Legado `trait VmBackend`** (ligado hoje à selecção/registo): ciclo de vida
  rico, métodos opcionais *fail-closed* — `compute/src/vm_backend.rs:526-994`.
- **Novo `trait VmProvider: Provider`** (5 verbos: create/start/stop/destroy/
  observe) + capacidades por nome — `compute/src/vm_provider.rs:151-206`.

Estado **não** é um único campo `status`: desejado (record `Vm`/`VmConfig`),
observado (recomputado: `is_running`/`ip`/`observe` → `VmObservation`), e
em-progresso (stages `CreateStage`/`DestroyStage`). Confiança do IP é um eixo
próprio (`IpConfidence::{Observed,Predicted,Unknown}`). Readiness é **em camadas**
(processo VMM → VM Running via `vm.info` → OS do guest via `guest_info` →
serviço do guest via `HotplugOutcome::Complete`). — `vm_provider.rs:170-192`,
`vm_backend.rs:400-498`.

**Selecção de backend sem fallback silencioso** (`compute/src/vm_registry.rs`):
nome explícito → `make_backend`+`require_capabilities`; senão `auto_detect` por
ordem de *seed* (cloud-hypervisor antes de libvirt). Um record com backend
irresolúvel → erro `UnregisteredBackendInRecord`, nunca um palpite
(`:325-348`). O default `vm default-backend` deste host é `libvirt`, mas o
caminho nativo é seleccionável explicitamente e **não cai** para libvirt.

## 3. Independência de libvirt (caminho nativo)

- Crate `delonix-provider-cloud-hypervisor`: **sem** dependência libvirt/virsh no
  `Cargo.toml`. Todas as menções a libvirt em `lib.rs` são comentários ou texto
  de erro que *sugere* `--backend libvirt` como alternativa — nunca o invoca.
- Quando uma funcionalidade falta (p.ex. volumes virtio-9p), o backend **recusa**
  com `Error::RequiresLibvirtBackend` (CH `:775-780`) em vez de cair para libvirt.
- Ao vivo: `ps`/`pgrep` confirmam zero `qemu`/`libvirtd` para a VM nativa.
  (Nota de ambiente: este host *tem* libvirtd activo para o laboratório do dono,
  mas ele não participa no caminho CH — a independência é no runtime path, não na
  ausência do pacote. Validação num host sem libvirt instalado fica para a fila.)

## 4. Falhas reproduzidas — e o que são de facto

| Observação inicial | Diagnóstico | Veredicto |
|---|---|---|
| `ping 10.200.254.151` do host → 100% perda | IP vive na **netns de infra rootless**; slirp4netns `--disable-host-loopback`; host não tem rota para 10.200/16 | **Esperado** (topologia), não bug |
| `ip link show vt<hex>` no host → "does not exist" | O tap está **dentro** da netns (`nsenter -t <holder> --user --net` mostra-o UP) | **Esperado**, não bug |
| Consola serial silenciosa | Socket serial *ao vivo*; ligámos após o boot — sem replay | **Esperado**, não bug |
| `ssh delonix@<ip>` sugerido falha (timeout) | **Hint incondicional** em `vm.rs:5487-5490`; no default rootless o IP não é alcançável do host — só `delonix vm ssh` o alcança | **BUG real** (ver fila #1) |

O guest, acedido pelo caminho correcto (`delonix vm ssh`), está **plenamente
operacional**: hostname+chave cloud-init aplicados, 2 vCPUs, ~2 GiB, disco
gravável, gateway alcançável, DNS/egress via slirp NAT.

## 5. Ciclo de vida — resultado ao vivo (harness, 28/28)

create+boot+cloud-init · guest exec · CPU/mem/disco/rede/DNS · stop→start
(machine-id estável, disco persiste) · restart (uptime reinicia, disco persiste)
· pause→unpause · cold resize 2→3 vCPUs (**guest confirma `nproc=3`**) · snapshot
(parado) criado+listado+removido · concorrência (2 creates paralelos → IPs **e**
MACs distintos) · **destroy sem órfãos** (processo, tap na netns, ficheiros,
record — todos limpos). Evidência: `evidence/harness-run-2026-10-24.txt`.

## 6. Classificação honesta (implementado/validado/parcial/não-suportado)

Ver `MATRIZ-CAPACIDADES.md`. Resumo: o catálogo de capacidades
(`compute/src/capability.rs`) já **é** o mecanismo pedido pelo briefing — 6
estados, evidência obrigatória com referência a teste verificada por um gate de
CLI, denominador que não pune recusas honestas. O problema **não** é
sobre-promessa; é **sub**-promessa: várias capacidades estão `partial` só porque
"nenhuma bateria com nome as prova" — e agora o harness prova-as.

## 6b. Correcções implementadas nesta sessão (patches prontos a aterrar)

Ambas preparadas em worktrees *detached* (sem consumir o WIP cap), compiladas e
verificadas; o `gitflow start`+commit fica para quando houver slot (ver §7).

- **`fix-vm-ssh-hint.patch`** (base `main` @ab4751ac) — `print_vm_next_steps`
  passa a liderar com `delonix vm ssh <nome>` e mantém `ssh <user>@<ip>` como
  fallback rotulado; tradução pt-AO incluída. `cargo check -p
  delonix-runtime-bin` verde.
- **`feat-vm-capability-graduation.patch`** (base `develop` @69c349c3) — gradua
  6 capacidades CH de `partial`→`supported` (`vm.destroy/restart/pause/resume/
  resize.cold` via checks `check "CH: …"` novos em `scripts/e2e.sh`;
  `vm.cloud-init` via `live:scripts/vm-native-ch-lifecycle.sh`, o harness
  tornado tracked). Matriz regenerada. **Validado:**
  - os dois testes-gate passam: `every_supported_capability_cites_evidence_that_exists`
    e `the_published_matrix_is_the_generated_one` (`cargo test -p delonix-runtime-bin`).
  - os checks VMM-level corridos AO VIVO com o binário novo: **14/14**.
  - discovery novo: vm-compute **12 supported / 6 partial** (era 6/12).

## 7. Fila priorizada de correcções (causas-raiz)

1. **[BUG] hint `ssh <ip>` enganador no caminho nativo rootless.**
   `bins/delonix-runtime-bin/src/cmd/vm.rs:5482-5493`: liderar com
   `delonix vm ssh <nome>` (funciona em todas as topologias, como já faz o ramo
   *appliance* em `:5499-5506`) e demover `ssh <user>@<ip>` a linha secundária
   rotulada "redes alcançáveis do host". Fix de **`main`** (bug), propaga por GitFlow.
2. **[VALIDAÇÃO] graduar capabilities `partial→supported`** com `evidence:`
   apontando a este harness, depois de o integrar como bateria com nome — alvos:
   `vm.destroy`, `vm.restart`, `vm.pause`, `vm.resume`, `vm.resize.cold`,
   `vm.cloud-init` (prova de login no guest). Toca o `match classify` do provider
   CH; requer o gate que verifica a referência de evidência (ver `capability.rs:21-24`).
   Feat de **`develop`**.
3. **[CI] integrar `harness-ch-lifecycle.sh` como bateria** condicionada a
   `/dev/kvm`+`cloud-hypervisor`+imagem golden; *skip* limpo quando ausente,
   falha quando presente e uma asserção quebra (§13.9/§16).
4. **[AMBIENTE] repetir o harness num host SEM libvirt instalado** para fechar a
   prova de independência total (§3, §16).

## 8. Critérios de aceitação (§16) — estado

| Critério | Estado |
|---|---|
| Funciona sem libvirt no ambiente de referência | ✔ (runtime path; falta host sem o pacote — fila #4) |
| Capacidades anunciadas com testes reais | ◑ harness prova o core; falta graduar (#2) e CI (#3) |
| Sem fallbacks ocultos | ✔ (`vm_registry.rs:325-348`) |
| Boot + readiness do guest comprovados | ✔ (harness A/A2) |
| Rede e persistência | ✔ (harness A2/B/C) |
| Lifecycle idempotente onde definido | ✔ (start idempotente; stop/start/restart) |
| Concorrência não duplica recursos | ✔ (IPs+MACs+taps distintos) |
| Reinícios/falhas com recuperação | ◑ reboot ✔; crash-recovery (guarda pid+starttime existe) por exercitar |
| Eliminação sem órfãos | ✔ (harness H) |
| Volumes persistentes não apagados | ✔ (destroy remove só o overlay/efémero da VM) |
| Segurança/permissões | ◑ /dev/kvm por grupo, argv estruturado (sem shell-injection), socket 0600; auditoria dedicada por fazer |
| Logs diagnosticam falhas | ✔ (`<vm>.log` + tail nos erros) |
| CI reproduz | ✗ pendente (#3) |
