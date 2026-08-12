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

## Medição por fazer, que precisa de autorização

Provar o VXLAN ao vivo exige `network create --driver overlay` — que muta o netns do holder onde
correm os 4 containers de produção deste host. **Não o fiz.** A classificação «realizado» acima
assenta no caminho de código e na validação registada no `AGENTS.md`, não numa medição desta
sessão. Com autorização, é uma rede descartável e um `network rm` a seguir.
