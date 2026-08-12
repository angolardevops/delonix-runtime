# Fase 0 — os dez fundamentos de virtualização, medidos

Levantamento pedido antes de qualquer desenho: tornar o runtime de VM do Delonix um IaC
autossustentável (sem Terraform), integrável com Terraform por SDK. A pergunta desta fase é uma só
— **o que é que este motor realmente fala, dos protocolos e padrões que toda a plataforma de
virtualização partilha?**

Medido em 2026-08-12 contra `delonix 0.50.0` (commit `00968bd8`), no host de desenvolvimento, com
4 containers de produção a correr (`nginx`, `pgvector:pg15`, `kaeso-odoo:18`, `pgvector:pg16`) e o
holder UP (pin 71944, controlo 306593, bridge `delonix0`, refcount 1). **Nenhuma medição mutou
estado**: todas as sondas são `plan`/`ls`/`describe`/grep.

## A tabela

| # | Fundamento | Estado | Prova |
|---|---|---|---|
| 1 | **TCP/IP** | **Realizado** | SDN viva: `kaeso-net` bridge `dlxnd2b6d4` 10.210.0.0/16; 4 containers Up, dois com porta publicada (`8080->80`, `5433->5432`); DNS interno com âmbito de namespace |
| 2 | **VLAN (802.1Q)** | **Não existe** | `grep -rniE '"vlan"\|type vlan\|vlan_id\|vlanId'` → **0** acertos fora de `macvlan`/`ipvlan` (substring) |
| 3 | **LACP** | **Não existe** | `lacp`, `bond`, `802.3ad` → **0** acertos em todo o `crates/` |
| 4 | **VXLAN** | **Realizado — e reportado como não realizado** | `infra::set_vxlan`/`do_vxlan` cria o device (`id <vni>`, `dstport 4789`, `nolearning`), masteriza-o na bridge e semeia o FDB; `network create --driver overlay` chama `realize_overlay`. **Mas ver o Achado 1** |
| 5 | **OVF/OVA** | **Não existe** | `ovf`, `\.ova` → **0** acertos. O `vm convert` faz qcow2/raw/vmdk/vdi/vhdx/vhd — formatos de **disco**, não o de **empacotamento** |
| 6 | **VirtIO** | **Realizado, parcialmente declarável** | XML libvirt emite `bus='virtio'`, `<model type='virtio'/>`, virtio-9p para volumes; CH idem. Recusa fail-closed correcta: `spec.volumes` num CH diz que precisa de libvirt (não há virtio-fs) |
| 7 | **NFS** | **Existe; impossível de montar neste host** | `kind: Volume` com `nfs:`, `mount -t nfs` em `delonix-volume`, provisionamento TrueNAS por API. **`CapEff: 0000000000000000`** e `mount.nfs` ausente → o mount falha aqui |
| 8 | **iSCSI** | **Não existe** | `iscsi`, `iscsiadm`, `multipath` → **0** acertos |
| 9 | **Fibre Channel** | **Não existe** | `fibre`, `fcoe` → **0** acertos |
| 10 | **NVMe over Fabrics** | **Não existe** | `nvme-of`, `nvmeof`, `nvme connect` → **0**. O único `nvme` no código é `/dev/nvme0n1` num teste de `--io-rate` |

**Fora da lista, mas da mesma família:** `macvlan`/`ipvlan` estão **só declarativamente** — no
`NetworkStore`, com aviso alto `Realized=False reason=DriverNotImplemented`. Isto é honesto e está
correcto: o plano físico deles precisa de `CAP_NET_ADMIN` na init-netns do host.

**Resumo: dos dez, três realizados (TCP/IP, VXLAN, VirtIO), um realizado mas inutilizável neste
host por falta de privilégio (NFS), seis inexistentes.**

## Achado 1 — o `stack plan` mente sobre o overlay (MÉDIO)

O único dos dez fundamentos de rede que este motor implementa é anunciado ao utilizador como não
implementado.

Reproduzido ao vivo, sem mutar nada:

```
$ delonix stack plan -f ov.yaml          # kind: Network, driver: overlay, vni: 42
  +   Network/fase0-overlay-probe
        prerequisite Realized: driver 'overlay' has no physical plane yet —
        it stays in the registry but containers only attach to `bridge`
```

A afirmação é falsa nas duas metades. `cmd/network.rs:572` diz o contrário no próprio comentário
(«Unlike macvlan/ipvlan, the overlay IS realizable without host privilege — it lives entirely in
the holder netns») e chama `realize_overlay`, que sobe bridge + uplink VXLAN + WireGuard. O
`AGENTS.md` regista a validação ao vivo. Quem está errado é `cmd/conditions.rs:186-192`, que mete
`overlay` no mesmo braço do `match` que `macvlan`/`ipvlan`.

**Alcance:** `conditions_for` alimenta três consumidores em `cmd/stack.rs` — o plano (linha 404,
impresso na 588), o `stack wait` (667) e `print_missing_conditions` (828). **Não bloqueia**: o
`wait` avisa uma vez e nunca espera por um prerequisito falhado, por desenho explícito. O dano é
de confiança, não de disponibilidade — mas é exactamente a classe «relato desonesto» que este repo
persegue, e recai sobre a funcionalidade de rede mais avançada que tem.

**Correcção:** tirar `"overlay"` do braço, dando-lhe condição própria — realizado, com a ressalva
medida de que o forwarding inter-nó nunca foi provado com um 2.º nó real. Teste que falhe com a
correcção revertida.

## Achado 2 — `kind: Vm` aceita 36 campos e converge 5 (ALTO)

Este é o obstáculo central ao objectivo «IaC autossustentável», e não é uma lacuna de feature: é
um caminho de aceitar-e-descartar em silêncio.

```
VM_SPEC_FIELDS      → 36 campos distintos aceites pelo manifesto
RECONCILED_VM_FIELDS → ["disk", "vcpus", "memory", "network", "backend"]
```

Reproduzido ao vivo contra uma VM existente (`lab`, Stopped, libvirt), com **seis** propriedades
declaradas que a VM não tem — `cpuTopology: 2×4×2`, `tpm: true`, `vnc: true`, `machine: q35`,
`bootOrder`, um `extraDisks` a apontar para um ficheiro inexistente e um `extraNics` para uma
bridge inexistente:

```
$ delonix stack plan -f vmdrift.yaml
  +~  Vm/lab  — exists and belongs to no stack — will be taken over
Summary: 1 to adopt
```

Zero alterações propostas. Um `apply` reportaria sucesso e a VM manteria a forma antiga.

**Controlo que torna isto conclusivo** — a ausência de aviso podia significar «campo não
reconhecido pelo plano». Não significa:

```
$ delonix stack plan -f vmbogus.yaml      # o mesmo, com um campo inventado
WARNING: Vm 'lab': unknown field 'campoQueNaoExiste' in spec — ignored (check the spelling)
```

O `warn_unknown_fields` **corre** no plano. Logo os seis campos acima são reconhecidos, parseados,
e **descartados pelo reconciliador** — a forma pior das três, porque o utilizador tem todas as
razões para julgar que foram aplicados.

**Causa-raiz, e é uma que já está catalogada neste repo.** O registo `Vm` persiste 10 campos
(`disk`, `overlay`, `vcpus`, `memory`, `network`, `tap`, `mac`, `restart_policy`, `namespace`,
`backend`, `devices`); a `VmConfig` que a criação consome tem ~30. Os 21 que faltam —
`kernel`/`initrd`/`firmware`/`cmdline`/`seed`/`hugepages`/`cpu_affinity`/`bridge`/`volumes`/`vnc`/
`static_ip`/`machine`/`cpu_model`/`cpu_topology`/`tpm`/`video`/`boot_order`/`extra_disks`/
`extra_nics`/`libvirt_xml_overlay`/`libvirt_xml` — **existem só durante o `vm create` e morrem com
ele**. O reconciliador não pode comparar o que o registo não guarda, e o `vm start`/`restart` já
documenta esta perda no próprio `--help`.

É a **quinta ocorrência** da armadilha: *estado necessário para RECONSTRUIR o recurso tem de ser
persistido, não só usado na criação* (antes: `-v` não persistido, `-p` em rede custom, redes
extra, `Container.pod`).

Note-se o que a lista dos 21 contém: topologia de CPU, TPM, ordem de arranque, discos e NICs
extra, IP estático, tipo de máquina. **É exactamente o vocabulário de uma plataforma de
virtualização.** O Delonix converge hoje o que um container precisa; o que uma VM precisa fica de
fora.

## O que isto responde sobre o objectivo

**Sem Terraform, hoje, não dá** — e a razão não é falta de um `terraform apply`. É que o objecto
declarativo de VM não retém nem compara aquilo que uma VM é. Um `stack apply` que promete
convergência e ignora 25 dos 30 campos não é substituível por Terraform porque é pior: reporta
sucesso.

**A ordem certa de trabalho é, portanto, o inverso da intuição.** Nenhum protocolo novo (VLAN,
iSCSI, OVA) vale nada antes de o Kind reter e comparar o que já aceita — senão acrescenta-se mais
vocabulário à mesma lista de campos silenciosamente descartados.

## Ordem proposta

1. **Achado 1** — barato, isolado, puro. Um braço de `match` e um teste.
2. **Achado 2, parte A: persistir.** Alargar o registo `Vm` aos campos que a `VmConfig` já aceita
   (`#[serde(default)]`, registos antigos continuam válidos — o precedente é o `namespace` da
   v0.40.0, com teste de regressão dedicado). Sem isto, o resto é impossível.
3. **Achado 2, parte B: comparar.** Alargar `RECONCILED_VM_FIELDS`, com o teste que este repo já
   exige por Kind — um manifesto inalterado tem de dar ZERO diferenças. Campos frios (que obrigam
   a recriar) têm de entrar no caminho `-/+` fail-closed, não convergir a quente.
4. **Só então** decidir quais dos seis fundamentos ausentes sobem ao manifesto — com o critério
   que a Fase 1 do prompt fixa: onde vive a realização de cada um, e o que é substrato do
   `delonix-deploy` em vez de trabalho deste repo.
5. **SDK/Terraform por último**, e a peça que o desbloqueia é `--format json` nas listagens: um
   provider que parseie tabelas cuja largura muda com o conteúdo e cujo texto muda com `--l18n=pt`
   nasce partido.

## Estado da execução

**Achado 1 — FECHADO** (`6fd74e9`). `overlay` saiu do braço do `macvlan`/`ipvlan`; ficou declarado
o pré-requisito real (um overlay cifrado precisa de `wg`), sondado pela MESMA `wg::available()` que
o realizador usa. O teste antigo percorria os três drivers a exigir `DriverNotImplemented` — fixava
o defeito — e foi substituído por dois que falham com a correcção revertida (verificado).

**Achado 2, parte A — FECHADO** (`43b3a85`). O registo `Vm` passou a guardar a forma de arranque
(`VmBootSpec`, no `core`, com os quatro tipos auxiliares movidos para lá e re-exportados pelo
`delonix-vm`). Bloco inteiro `skip_serializing_if`: uma VM sem opções avançadas não cresce um byte,
e um registo antigo não tem a chave. Ausente é **desconhecido**, não «não tinha nenhum».

A guarda que impede a reincidência: `boot_spec_of` desestrutura a `VmConfig` exaustivamente e o
`config_from` constrói-a sem `..Default::default()` — um campo novo parte a build nos dois sítios.
Verificado a acrescentar um campo de teste. Foi precisamente o `..Default::default()` que deixou
21 campos virarem defaults em silêncio a cada reinício.

Validado ao vivo: os três registos deste host escritos pelo binário anterior continuam a ler-se,
`vm ls`/`describe` intactos, e nenhum ganhou a chave só por ser lido.

## Achado 2, parte B — «comparar os seguros» tem hoje o conjunto VAZIO

A escolha era comparar os campos que não obrigam a recriar. Ao implementá-la, o conjunto revelou-se
vazio, e a razão é estrutural:

- `Action::Update` está definido como «converge sem recriar **e sem mudar o PID**». Este motor não
  faz hotplug: nenhum destes campos se aplica a uma VM a correr.
- `Action::Replace` destrói e recria — e recriar deita fora o overlay, ou seja tudo o que o guest
  escreveu. É por isso que é recusado sem `--replace`.
- `create_with` devolve cedo quando a VM já corre (`return Ok(ex.clone())`, idempotente), logo um
  `apply` sobre uma VM viva **não faz nada** mesmo agora que a forma está persistida.

Alargar o `RECONCILED_VM_FIELDS` sob este modelo transformaria cada campo novo num `Replace`
destrutivo — exactamente o que a escolha queria evitar. Ou seja: comparar mais campos hoje é
comparar para propor perda de dados.

**O que a parte A destrancou, e que não existia antes**: uma terceira classe entre as duas. Mudar
`tpm`/`machine`/`cpuTopology`/`vnc`/`bootOrder`/`extraDisks`/`extraNics`/`video`/`cpuModel` é
aplicável por **stop + start** — o PID muda, mas o overlay sobrevive, e agora que a forma está no
registo o arranque seguinte usa-a de facto. Não é `Update` (o PID muda) nem `Replace` (não há perda
de dados): é reinício.

Acrescentar essa classe mexe no `enum Action`, que faz parte do payload `-o json` — um contrato que
o ADR-0005 existe para manter estável. **Por isso a parte B pára aqui e passa a ADR**, à disciplina
que este repo aplica a fronteiras estruturais. As perguntas a fechar nesse ADR: um `apply` pode
reiniciar uma VM sem ser pedido (é interrupção de serviço, não perda de dados)? Se não, qual é o
gate — um `--reboot` a par do `--replace`? E o que faz o `--detailed-exitcode` com uma mudança que
o utilizador ainda não autorizou.

## Medição por fazer, que precisa de autorização

Provar o VXLAN ao vivo exige `network create --driver overlay` — que muta o netns do holder onde
correm os 4 containers de produção deste host. **Não o fiz.** A classificação «realizado» acima
assenta no caminho de código e na validação registada no `AGENTS.md`, não numa medição desta
sessão. Com autorização, é uma rede descartável e um `network rm` a seguir.
