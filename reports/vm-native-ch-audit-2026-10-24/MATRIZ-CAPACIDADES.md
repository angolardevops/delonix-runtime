# Matriz de compatibilidade e capacidades — backend nativo Cloud Hypervisor

Configuração de **referência** (única validada; não prometer para além dela).
Dump bruto do discovery: `evidence/ch-capabilities-raw.txt`.
Factos do host: `evidence/reference-config.txt`.

## Configuração de referência fixada

| Eixo | Valor |
|---|---|
| CPU / arch | Intel Core i9-13900H, x86_64, flag `vmx`, nested=`Y` |
| Kernel do host | 7.0.0-38-generic |
| KVM | `/dev/kvm` presente, acesso por grupo `kvm` (rootless) |
| VMM | Cloud Hypervisor **v52.0** (`/usr/local/bin/cloud-hypervisor`) |
| Firmware | EDK2 `CLOUDHV.fd` (4 MiB, `/usr/local/share/delonix/`) |
| Delonix | 5.1.1 / commit `f49d3570c` |
| Guest | Ubuntu 24.04 (`k8s-golden`), kernel 6.8.0-134, cloud-init |
| Formato de imagem | qcow2 (overlay com backing file, `image_type=qcow2`) |
| Rede | SDN rootless: netns de infra + `slirp4netns`, tap na bridge `ingress` |
| Armazenamento | overlay qcow2 por-VM + seed ISO NoCloud |
| Modo | **rootless** (sem root para o caminho provado) |

> Fora desta matriz nada é prometido. Arquitecturas não-x86, outras versões de
> CH, boot por kernel directo, outros formatos (raw/vmdk/…) e hosts sem libvirt
> instalado **não** foram validados nesta sessão.

## Capacidades vm-compute do backend CH (do discovery) e prova do harness

Legenda de estado = estado **declarado hoje** pelo catálogo. "Harness" = o que
`harness-ch-lifecycle.sh` provou ao vivo nesta sessão (28/28).

| Capacidade | Estado hoje | Harness provou | Acção proposta |
|---|---|---|---|
| vm.create | supported | ✔ A | — |
| vm.start | supported | ✔ A/B | — |
| vm.stop | supported | ✔ B | — |
| vm.resume-same-identity | supported | ✔ B (machine-id estável) | — |
| vm.disk.resize | supported | (coberto por testes do provider) | — |
| vm.namespace-isolation | supported | (chaos existente) | — |
| **vm.destroy** | partial | ✔ H (zero órfãos) | → graduar c/ `evidence:` |
| **vm.restart** | partial | ✔ C (uptime reinicia) | → graduar |
| **vm.pause** | partial | ✔ D | → graduar |
| **vm.resume** | partial | ✔ D | → graduar |
| **vm.resize.cold** | partial | ✔ E (guest vê 3 vCPUs) | → graduar |
| **vm.cloud-init** | partial ("nunca faz login no guest") | ✔ A (hostname+chave, login real) | → graduar (prova de login) |
| vm.hotplug.cpu.add | partial | não testado (precisa `--vcpus-max`) | manter partial; testar c/ headroom |
| vm.hotplug.memory.add | partial | não testado (precisa `--memory-max`) | manter partial |
| vm.cpu.pinning | partial | não bootado | manter partial |
| vm.memory.hugepages | partial | só unit | manter partial |
| vm.device.pci-passthrough | partial | sem IOMMU no host | manter partial |
| vm.antispoof | partial | tabela `dlxspoof` existe; forja não testada | manter partial |
| vm.clone / vm.template | not-implemented | — | não prometer |
| vm.hotplug.{cpu.remove,memory.remove,disk.*,nic.*} | not-implemented | — | não prometer |
| vm.disks.extra / vm.nics.extra | unsupported-by-provider | — | recusa correcta (1 disco+1 tap) |
| vm.device.tpm / vm.cpu.model / vm.raw-definition | unsupported-by-provider | — | recusa correcta |
| vm.restart-policy.native | unsupported-by-provider (supervisor do motor trata) | — | recusa correcta |

**Contagem vm-compute:** 6 supported · 12 partial · 8 not-implemented · 6
unsupported-by-provider. O harness dá evidência viva a **6** das 12 `partial`;
graduá-las (fila #2 da auditoria) move a maturidade do backend sem mentir.

## Snapshot (domínio Protection)

CH faz snapshot **apenas de VM parada** (o VMM a correr detém o disco em
exclusivo; CH não tem API de snapshot de disco ao vivo) — documentado em
`vm snapshot --help`. Harness: create+ls+rm de VM parada **passou**.
Snapshot de VM a correr / com memória: **não suportado** por este backend
(é de libvirt) — recusa com mensagem, não silêncio.

## O que NÃO prometer (rejeitado com erro, ausente do discovery como "supported")

live migration de VM, snapshot completo (memória) em CH, GPU passthrough sem
IOMMU, hotplug de disco/nic, discos/nics extra, vTPM, selecção de modelo de CPU,
definição raw. Todos aparecem como `not-implemented` ou `unsupported-by-provider`
no discovery — nunca como `supported`.
