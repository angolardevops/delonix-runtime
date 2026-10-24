# Plano de sprints — campanha de conformidade de capacidades (baseline v5.1.1)

Deriva de `matrix-structured.json`. **158 entradas accionáveis** (partial + not-implemented); os restantes 357 estados são estruturais (230 unsupported-by-provider, 12 requires-external-component, 3 unavailable-on-host, 112 já supported).

## Distribuição das 158 accionáveis

Por domínio: vm-compute 45 · network 40 · metrics 13 · firewall 13 · inventory 12 · protection 7 · console 7 · access 6 · storage 5 · containers 4 · system-containers 4 · mobility 2.

Por provider: proxmox 48 · libvirt 39 · linux 32 · cloud-hypervisor 27 · opnsense 12.

`not-implemented` (as lacunas mais limpas, 72): concentram-se em vm-compute (CH 8, libvirt 6, proxmox 6), network (proxmox 12, opnsense 11, linux 2), metrics (proxmox 4), inventory e console. Ver o detalhe em `matrix-structured.json`.

## Regra de promoção (critério #16, não negociável)

Mudar uma capability para `supported` exige, cumulativamente: implementação funcional ligada aos caminhos públicos (CLI/API/Desktop coerentes) · validação de pré-condições · erros accionáveis · persistência/recuperação quando aplicável · **teste real** (a evidência `check:`/`e2e:`/`live:`/`test:`/`chaos:` tem de existir e ser verificável) · documentação · evidência ligada a commit+ambiente. Sem ambiente, conclui-se implementação+testes locais mas mantém-se **"implementado, não validado"** na dimensão de evidência (fica `partial` com limite escrito, nunca `supported` falso).

## Sequência por dependências

### Sprint 0 — reconciliar (ESTE checkpoint) ✅
Baseline, binário, matriz 515 lossless, diff explicado, plano. Entregue em `develop` (docs-only).

### Sprint 1 — riscos de isolamento, dados e classificação (maior risco primeiro)
Antes de somar features, corrigir o que engana ou arrisca dados:
- **Classificação errada**: recusas no adapter marcadas como ausência upstream; capabilities de um domínio atribuídas a provider errado; ADRs tratados como impedimento permanente. Reclassificar com fundamentação (não conta como feature nova).
- **Isolamento**: namespaces partilhados (IPC/UTS/PID) verificados por identidade de processo real, não por `setns` ter sido chamado; motivo de término OOM distinto de outros términos.
- **Dados**: snapshots/backups que só provam criação de ficheiro — exigir teste de restauro+consistência antes de qualquer promoção.
- Especialistas: Rust + segurança + testes. Baixo risco de ambiente (quase tudo testável no host rootless live).

### Sprint 2 — completar o que existe mas não está validado
Entradas `partial` cujo backend já existe e só falta prova live:
- vm-compute libvirt/CH: operações testadas só por XML/args/mock → re-testar com guests reais (hotplug disco com R/W no guest, NIC com tráfego, snapshot restaurado com dados).
- network linux: IPAM/reservas/DNS/estático com verificação de dataplane (tráfego permitido/negado), não só existência de regra.
- Especialistas: virtualização + networking + testes. Ambiente: host live (libvirt/CH provados aqui); Proxmox via nó `pve` em virbr0.

### Sprints de domínio (3+) — implementar lacunas por frente, em paralelo quando independentes
- **Compute CH/libvirt** (mandato §7): eventos, async, clone/templates, resize, hotplug/unplug (4 níveis: aceite→aplicado→detectado no guest→utilizável), discos/NICs extra, CPU model/pinning, hugepages, TPM/PCI passthrough, cloud-init, guest agent, IP observado, snapshots, backup/restauro, migração, consola, métricas/capacidade.
- **Proxmox** (§8): jobs/UPIDs, templates, resize de disco, IP estático, storage pools, snapshots persistentes, consola serial/VNC, métricas; system containers (import, rede, firewall, backup/restauro, exec/logs/exit via mecanismo suportado — sem inventar endpoints; LXC ≠ OCI).
- **Rede linux** (§10): DHCP de VMs no ledger IPAM, reservas sem colisão sob concorrência, estático, DNS, IPv6 ponta-a-ponta, VXLAN, overlay encriptado (≥2 nodes), proxy L7, LB L4, healthchecks, drift/dataplane, firewall logging com limites.
- **Rede Proxmox + OPNsense** (§11): SDN zones, VLAN/VXLAN, subnets/IPAM/DHCP/DNS/SNAT/IPv6, firewall VM/CT com tráfego provado; OPNsense regras com identidade, multi-WAN, VPN, NAT 1:1, NPT, rollback, dataplane — sem activar switches globais nem instalar plugins sem autorização específica.
- **Storage** (§12): NFS/CIFS/WebDAV/LVM-thin/ZFS/Btrfs/TrueNAS/Ceph, pools, replicação, HA — distinguir falta de privilégio de falta de implementação; helpers restritos em vez de root global; HA e replicação têm contratos próprios (restart local ≠ HA).
- **Containers/pods** (§9): eventos streaming, async, GPU/CDI (só com hardware), OOM, namespaces partilhados, métricas, capacidade, resolução de secrets.

### Sprint final — regressão integrada + novo inventário
Re-correr `delonix provider matrix`/`ls`, regenerar `matrix-structured.json`, produzir matriz antes/depois com todas as 515 entradas e as contagens separadas (novas implementadas / completadas / validações / reclassificações / dependências integradas / bloqueios de ambiente / limites upstream comprovados). Progresso **não** se mede só pela redução de estados negativos.

## Harness por frente

Uma branch+worktree por frente, a partir de `develop` (features/manutenção) ou `main` (fixes/bugs), via `scripts/gitflow.py start`. Revisão separada antes de integrar. Integração em develop após gates verdes (`make gitflow gates`). Limpeza segura no fim. Frentes independentes em paralelo (worktrees distintas); alterações a contratos comuns (catálogo `capability.rs`, protos) coordenadas antes de distribuir. Nunca duas baterias a disputar o mesmo recurso de teste (um nó `pve`, um host GPU).

## Ambientes autorizados conhecidos

- Host live rootless (este): containers OCI, pods, rede linux, VM libvirt+CH, cgroup2 cpu/memory/pids delegados. io/cpuset não delegados.
- Nó Proxmox `pve` pré-instalado em virbr0 (root@pam / default público) — create/snapshot/destroy provados pelo provider.
- Sem hardware GPU/TPM/PCI-passthrough confirmado → essas capabilities ficam "implementado, não validado".
- OPNsense/multi-node overlay: a confirmar por frente antes de prometer validação live.
