# 09 — VMs (providers) e APIs/MCP

SUT: `delonix 5.1.1` @ `bca34f6c3`.

## Providers (inventário medido — `delonix provider ls`)

| Provider | Kind | Disponível | Saúde | Supported caps |
|---|---|---|---|---|
| libvirt | compute | **sim** | Ok | 18 |
| cloud-hypervisor | compute | **sim** | Ok | 13 |
| linux | compute/network/storage | sim | Ok / NetworkSharesNeedRoot | 11/10/4 |
| proxmox | compute/network | **não** | NotConfigured | 33/7 |
| opnsense | gateway | não | NotConfigured | 10 |

Imagens VM locais: **`k8s-golden`** (ubuntu 24.04, cloud-init, k8s 1.34, 634 MiB) e `delonix-vm-appliances:proxmox-ve-9.2` (1.45 GiB).

## VM-1 — libvirt/KVM, ciclo completo (verificação independente em `virsh`)

| Passo (§10) | Observado | Estado |
|---|---|---|
| criar+arrancar | `vm create relcvm --disk k8s-golden --backend libvirt --vcpus 2 --memory 2G` → overlay+rede+domínio+start | **PASSOU** |
| guest arranca + rede/DNS | `vm ls`: Running, **IP 192.168.122.62** (DHCP libvirt, ~34s); **`virsh list` confirma domínio `relcvm running`** | **PASSOU** |
| reachability | `ping` **2/2, 0% perda** | **PASSOU** |
| stop | `vm stop` → `virsh` mostra shut off | **PASSOU** |
| start | `vm start` → `virsh` Id 3 running | **PASSOU** |
| destroy + limpeza | `vm destroy --force` remove `relcvm.qcow2`, overlay dir, `relcvm.xml`; **0 em delonix E 0 em libvirt** | **PASSOU** |
| preservação | `opnsense` continua running, `pve92` shut off — intactos | **PASSOU** |

## VM-2/VM-3/VM-4 — estado

- **cloud-hypervisor (VM-2):** provider available/Ok (13 caps); backend existe; **ciclo não corrido** nesta ronda (tempo/disco) → NÃO EXECUTADO.
- **proxmox (VM-3):** `NotConfigured` — exige `DELONIX_PROXMOX_URL/_NODE`+credencial e um PVE vivo (`pve92` desligada). **BLOQUEADO (ambiente)**.
- **build de imagem cloud (VM-4, §11):** não corrido — disco a 96%/44 G. **BLOQUEADO (ambiente)**. Nota: a golden `k8s-golden` já existe e arranca (provado acima), o que cobre o *boot* de uma imagem cloud-init; o *fluxo de build* em si não foi exercitado.

## API-2 — MCP (closes folhas gap E3: `tools/call` nunca correra)

Sidecar `delonix-mcp` construído **no mesmo commit** (o binário principal **recusa** uma cópia stale do PATH — `DX-6000`, boa defesa anti-mistura).

- `mcp capabilities`: **tabela de risco/confirm** — cada tool tem `risk` (READ/…) e `requires_confirm`.
- `mcp serve` + JSON-RPC `initialize`+`tools/list`: **15 tools** devolvidas (bate com o denominador `mcp=15`): `audit.query, container.restart, logs.query, metrics.query, network.inspect, resource.{list,get,describe}, resources.get, runtime.{health,info}, storage.inspect, task.{cancel,get}, workload.usage`.
- Conjunto **dominado por leitura/observação**; as únicas mutações são `container.restart` e `task.cancel` — **nenhum exec privilegiado arbitrário**. Satisfaz "MCP não contorna o núcleo nem aceita comandos privilegiados arbitrários". | **PASSOU** |

## API-1 — CLI vs REST vs gRPC vs MCP — PARCIAL

Validada a superfície **MCP** pelo protocolo (initialize+tools/list) e a **CLI** exaustivamente nas
secções anteriores. A comparação ponta-a-ponta das 4 superfícies sobre a MESMA operação (identidade,
autz, defaults, erros) **não foi corrida** — exigiria servir `node-api`/`mgmt`/`docker-api` do próprio
SUT (mais sidecars/arranque). O serviço `delonix-engine prod` em `0.0.0.0:9443` é de **outra instalação**,
não o SUT, e não foi usado. → cobertura parcial.
