# 59 — P4/D1-D4: o spike de substituição (ADR-0044)

**Data:** 2026-09-18 · **Base:** `origin/main` (`6c063295`, v4.0.0 +9 commits) ·
**Veredicto: GO.** A mesma `VmSpec` converge Cloud Hypervisor e libvirt de ponta a ponta,
ao vivo, com zero branching por nome de backend fora do registo — o portão que este ADR
exige antes de "Accepted" para o D1-D4.

## A pergunta

O spike nº1 do ADR-0044: "Hand the *same* `VmSpec` to `delonix-provider-cloud-hypervisor`
and `delonix-provider-libvirt` and converge both — `delonix vm create`, `stop`, `start` on
each, from one manifest, with **zero** `#[cfg]`/name-branching outside the composition
root. Measure by `grep -rn 'backend.contains\|backend\.id() =='` returning zero outside
`bins/`, not by inspection."

## Como se mediu

- **`crates/adapters/delonix-vm/src/provider_spike.rs`** (novo, `pub mod`, marcado SPIKE):
  `VmSpec`/`Extensions`/`CloudHypervisorExt`/`LibvirtExt` (D1/D2), `Provider`/`VmProvider`
  (D3), `LocalVmProvider` + `registry(id)` (D4). **Reaproveita `create_with`/`start`/
  `stop`/`status`/`remove` verbatim** — o contrato deste spike é a FORMA (`VmSpec`+
  `Extensions` → `VmConfig`, uma chamada por operação, zero branching por nome fora do
  registo), não um segundo motor de orquestração ao lado do que já existe, testado, e em
  que este repo já confia.
- **`crates/adapters/delonix-vm/tests/provider_spike_live.rs`** (novo, `#[ignore]`): a
  MESMA `VmSpec` através de `registry("cloud-hypervisor")`/`registry("libvirt")`,
  create→observe→stop→observe→start→observe→destroy, root isolado (`tempdir`), disco
  base `qemu-img create` de 64 MiB partilhado pelos dois, `cloud_init: Some(false)` (sem
  SO convidado — este spike prova mecânica, não um guest a sério, mesma escolha já feita
  para o spike do Proxmox, `58_...md`).
- **A classificação de campos foi MEDIDA, não copiada da tabela do D1** — `grep`/leitura
  directa de `boot_ch`/`libvirt_domain_xml` em vez de confiar na tabela do ADR original
  (ver "Achados", abaixo).

## O que se mediu

**libvirt: convergência completa, através do binário de teste `cargo test`**:
```
=== libvirt ===
  created
  after-create: running=true ip=None confidence=Unknown
  after-stop: running=false
  after-start: running=true
  destroyed
```

**Cloud Hypervisor: bloqueado no binário de teste por uma razão estrutural, não deste
port** — `no VM network provider is registered in this process`. `delonix_vm::
set_network(...)` só é chamado pelo `main.rs` real; um binário `cargo test` nunca corre
`main`. Corrigido temporariamente registando o port no teste (precisou de
`delonix-sdn` como dev-dependency — revertido depois, ver "O que ficou", abaixo) e
medido de novo: **um SEGUNDO bloqueio, este genuíno e sem correcção barata**: o re-exec
do `netns pin` usa `std::env::current_exe()`, que num binário de teste resolve para o
PRÓPRIO binário de teste, não para o `delonix`. Sem um `DELONIX_BIN`-style override para
este caminho (existe para OUTROS self-execs, `delonix_node::dispatch::cli_bin`, mas não
para o pin/holder da SDN), o CH não pode completar o seu ciclo de vida a partir de um
`cargo test`.

**A prova de CH ficou completa através do binário `delonix` REAL**, por um verbo oculto
temporário (`DLX_P4SPIKE=1 delonix version`, revertido — ver "O que ficou"):
```
$ DLX_P4SPIKE=1 delonix version
P4SPIKE: created
P4SPIKE: after-create running=true ip=Some("10.200.254.164") confidence=Predicted
P4SPIKE: after-stop running=false
P4SPIKE: after-start running=true
P4SPIKE: destroyed
```

**Confirmado limpo depois**: sem lease órfão no IPAM, sem VM residual, sem regra `nft`
residual, containers de produção intocados (verificado com `delonix network ipam ls`/
`delonix vm ls`/`nft list ruleset`/`delonix container ls` contra o host real).

**A medição do próprio ADR** (`grep -rn 'backend.contains\|backend\.id() =='
--include=*.rs crates/`) continua em **1 hit, dentro de um comentário** — o mesmo de
antes deste spike. O novo módulo não introduz nenhum branching por nome fora de
`registry(id)`, a única função onde uma string é comparada.

## Achados — o D1 tinha dois campos mal classificados

Medido por leitura directa de `boot_ch` (linhas 1907-2177) e `libvirt_domain_xml`
(2590-2957), não pela tabela do ADR:

1. **`devices` (VFIO passthrough) É partilhado por CH e libvirt** — a tabela original do
   D1 dizia "not universal even between the two local backends — libvirt-only in practice
   today". Falso: `boot_ch` itera `cfg.devices` para `--device path=…` (linha 1968) e
   `libvirt_domain_xml` itera o MESMO campo para `<hostdev>` (linha 2922) — o mesmo
   `Vec<String>` de caminhos sysfs, format idêntico. Fica duplicado em
   `CloudHypervisorExt`/`LibvirtExt` (não promovido a `VmSpec`, porque o Proxmox recusa-o
   por inteiro — não é universal a TODOS os providers, só aos dois locais) — uma
   divergência documentada no próprio módulo, com a via honesta (um tier `LocalExt`
   partilhado por CH+libvirt, ausente para providers remotos) nomeada e não construída,
   fora do âmbito deste spike.
2. **`serial_capture` estava na tabela do D1, mas mal classificado — lumped na lista de
   campos "refused by Proxmox" (destino `Extensions`), como se fosse específico de
   libvirt.** É universal. `boot_ch`
   (linhas 1980/1986) e `libvirt_domain_xml` (linha 2578) leem-no identicamente: "captura
   a consola para um ficheiro em vez de a expor interactivamente" não é uma noção
   específica de nenhum dos dois. Entra em `VmSpec`.

## Achados operacionais — dois bloqueios reais que uma leitura nunca revelaria

3. **`network: "default"` não é o valor por omissão da CLI — é `"ingress"`.** A primeira
   tentativa deste spike usou `"default"` (por analogia com `namespace: "default"`) e
   recebeu `no such ingress network 'default' does not exist`. `cmd/vm.rs:519` mostra o
   `default_value` real: `"ingress"`. Um engano fácil de repetir para quem desenhar contra
   `VmSpec` sem correr nada.
4. **O re-exec do `netns pin` não tem override para `current_exe()`, ao contrário de
   OUTROS self-execs deste motor.** `delonix_node::dispatch::CLI_BIN_ENV` (`DELONIX_BIN`)
   existe precisamente para "um servidor que corre a CLI de volta" — mas
   `delonix-sdn::infra::start_control` (e os outros dois `current_exe()` do mesmo
   ficheiro) não o consultam. Isto é DIRECTAMENTE relevante ao spike nº2 (o par de
   binários `delonix-launcher`/`delonix-netns-holder`, P4d): se o holder passar a ser um
   binário PRÓPRIO, a pergunta "que binário se re-executa a si mesmo" deixa de ter uma
   resposta implícita (`current_exe()` == `delonix`) e passa a precisar do mesmo
   `DELONIX_BIN`-style override que `delonix_node::dispatch` já resolveu para outro
   caminho. **Achado que se transporta para o spike nº2**, não corrigido aqui.

## O que ficou

**Ficou no repositório** (não é "spike, não fundir" desta vez — ver a razão): o módulo
`provider_spike.rs` inteiro (tipos + `spec_to_config` + `registry` + 5 testes puros) e o
teste ao vivo `tests/provider_spike_live.rs` (a metade de libvirt fica genuinamente verde
em CI; a de CH fica SKIP com a razão exacta documentada, nunca um erro opaco). Ao
contrário dos spikes D5/D6 (uma exploração descartável), este código NÃO duplica nada —
é uma casca fina sobre `create_with`/`stop`/`start`/`status`/`remove` já testados, não
introduz risco à superfície pública existente, e é directamente o início real do que P4b
tem de construir. Manter isto é trabalho poupado, não dívida.

**Revertido, não fica**: a dependência `delonix-sdn` como `[dev-dependencies]` de
`delonix-vm` (reintroduzia exactamente a dependência que este port existe para remover —
ADR-0040 P3 — sem resolver o bloqueio de `current_exe()` de qualquer forma); o verbo
oculto `DLX_P4SPIKE` em `main.rs` + `cmd::vm::p4_substitution_spike_ch()` (investigação
pontual, não uma capacidade a expor). O relatório acima cita as suas saídas textuais como
prova; o código que as produziu não precisa de sobreviver a esta sessão.

## Proven vs not validated

**Proven**: convergência completa CH+libvirt através da MESMA `VmSpec`, ao vivo, nos dois
sentidos do ciclo (stop→start); zero branching por nome fora de `registry`; as duas
correcções ao D1 (`devices`, `serial_capture`); o valor real do default `network`; o
bloqueio de `current_exe()` no binário de teste, e a sua ausência de override.

**Not validated**: `Extensions` com conteúdo real (este spike usou `Extensions::default()`
nos dois lados, deliberadamente, para isolar a convergência dos campos universais —
`spec_to_config`'s testes puros cobrem o mapeamento de `CloudHypervisorExt`/`LibvirtExt`,
mas não uma VM a arrancar com eles); um guest a sério (SO instalado, cloud-init real);
Proxmox (fora de âmbito, P4c); a retirada de `delonix-vm` como nome de crate e a divisão
em `delonix-provider-cloud-hypervisor`/`delonix-provider-libvirt` (D2.3/D2.5) — este spike
prova que a FORMA funciona, não substitui o trabalho real de mover ficheiros e crates que
P4b ainda tem de fazer.
