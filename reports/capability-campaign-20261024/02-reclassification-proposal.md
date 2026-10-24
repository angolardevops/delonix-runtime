# Sprint 1 — Proposta de reclassificação (auditoria §4)

Data: 2026-10-24. Frente read-only do Sprint 1. Dois auditores especializados (compute; rede/storage/gateway) cruzaram as 350 entradas partial/not-implemented/unsupported dos 5 providers contra o código dos adapters e a documentação oficial. **Reclassificar não conta como funcionalidade implementada** (mandato §13) — é correcção da causa (§4). A aplicação em código fica para uma worktree off develop (ver efeitos-cascata no fim); este documento é a proposta verificada.

Resultado global: a matriz está bem construída. Dos ~350 estados negativos, só **3 têm fundamento alto para mudar de estado** (todos auto-verificados abaixo), mais um punhado de confiança média/textual que exige prova live antes de mexer.

## A. Confiança ALTA — verificado directamente nesta sessão

### A1. libvirt | `vm.migration.live` : `unsupported-by-provider` → `not-implemented`
- **Causa §4.1**: limitação arquitectural do engine tratada como ausência upstream.
- **Código** (`crates/providers/delonix-provider-libvirt/src/lib.rs`, arm `C::VmMigrationLive`): `reason: "ADR-0031: needs shared VM storage or a privileged libvirt daemon listening on the network; neither is a default of this engine"`. A própria razão descreve **caminhos existentes** (storage partilhado *ou* daemon privilegiado) — a capacidade é do libvirt, o que falta é o engine montá-la.
- **Fonte oficial**: libvirt.org/migration.html — migração ao vivo sem storage partilhado é suportada via `--copy-storage-all` desde libvirt 1.0.3.
- **Contraste que valida o critério**: o mesmo capability no cloud-hypervisor (`S::UnsupportedByProvider`, "CH migrates memory only, never the disk") **mantém-se correcto** — bate à letra com `docs/live_migration.md` do CH; aí é limite real do provider. A reclassificação é só do libvirt.
- **Nota**: `not-implemented` é mais honesto que `unsupported-by-provider`; alternativa defensável = `requires-external-component { component: "shared/replicated VM storage (ADR-0031)" }`, coerente com o irmão `C::VmReplication` que já usa esse estado. Decidir na aplicação consoante se o engine pretende suportar `--copy-storage-all` nativamente (→ not-implemented) ou exigir storage externo (→ requires-external).

### A2. linux | `storage.lvm-thin` : `not-implemented` → `partial`
- **Causa §4.4**: not-implemented quando já está implementado e ligado ao caminho público.
- **Código**: `crates/adapters/delonix-volume/src/provider_report.rs:101` → `C::StorageLvmThin => S::NotImplemented`. Mas o driver existe (`crates/providers/delonix-provider-lvm/src/lib.rs`, 621 linhas, testes próprios), é **registado no arranque** (`bins/delonix-runtime-bin/src/cmd/storage_pool.rs:134` via `register_drivers()`), está na **allowlist** (`crates/contexts/delonix-storage/src/allowlist.rs:47` — `("lvm-thin","P4")`) e serve alocação de volumes pelo comando público `storage pool`. Teste de arquitectura `every_pool_driver_the_allowlist_accepts_is_registered` (`storage_pool.rs:1090`) prova o encadeamento.
- **Estado correcto** = `partial` com limite escrito: *"driver implementado, registado e aceite pela allowlist; sem prova e2e num pool LVM-thin real (scripts/e2e.sh só exercita o driver `dir`)"*. Não é `supported` (falta evidência live — critério #16).

### A3. linux | `storage.zfs-btrfs` : `not-implemented` → `partial`
- **Causa §4.4**: idêntica a A2.
- **Código**: `provider_report.rs:102` → `C::StorageZfsBtrfs => S::NotImplemented`. Drivers em `delonix-provider-zfs/src/lib.rs` (477 linhas) e `delonix-provider-btrfs/src/lib.rs` (498 linhas), ambos registados (`storage_pool.rs:125,130`), na allowlist (`("btrfs","P2")`, `("zfs","P3")`), com testes unitários.
- **Estado correcto** = `partial` com o mesmo limite ("sem prova e2e em pool real").

## B. Confiança MÉDIA — exige verificação live antes de mexer (candidatos a Sprint 2, não a reclassificação cega)

| provider\|capability | actual → proposto | causa | o que verificar antes |
|---|---|---|---|
| cloud-hypervisor\|`vm.network.bridge` | unsupported → not-implemented | §4.1 — adapter ignora `net_mode` (nunca lido); libvirt implementa o mesmo modelo tap+bridge como partial | confirmar que o tap do CH é de facto indiferente à bridge e que só falta fiação SDN |
| cloud-hypervisor\|`vm.ip.observed` | unsupported → partial | §4.1/§4.5 — caminho ARP `--wait` já existe (`lib.rs:471`), exercitado em `vm.antispoof` | confirmar que o resultado do ARP é escrito no campo "IP observado" que `vm show`/`ls` lê, e não isolado no anti-spoof |
| linux\|`net.apply.rollback` | not-implemented → unsupported-by-provider | §4.3/§4.6 — `nft -f` substitui o ruleset atomicamente; nada fica staged (ADR-0059) → rollback é inaplicável, não pendente. Coerente com `net.apply.staged` já unsupported | confirmar leitura de ADR-0059 §152/§248 e decisão de produto |
| linux\|`volume.webdav` | unavailable-on-host (state mantém) — justificação partilhada com NFS/CIFS tecnicamente duvidosa | §4.1 — `mount.davfs` é FUSE (helper setuid `fusermount`), não precisa de `CAP_SYS_ADMIN` como NFS/CIFS | confirmar no host se `mount -t davfs` monta rootless; se sim, separar a justificação de WebDAV das de NFS/CIFS |

## C. Precisão textual (não muda `state`, melhora a honestidade da interface — mandato §13)

- **proxmox | `net.gateway.*`** (bloco firewall ~`lib.rs:7131`): o texto "the node's SDN has no perimeter filter of its own" é factualmente falso — o PVE tem Datacenter Firewall com política de perímetro por omissão. A decisão de não a gerir (administração, ADR-0049 D3) pode manter-se, mas o texto deve dizer "existe, o engine não lhe toca por desenho", não negar a existência.
- **proxmox | `vm.raw-definition`** (unsupported, ADR-0049 D6): existe `args:` cru no `qm`/API (análogo parcial ao `libvirtXml` que o libvirt expõe como partial). Âmbito genuinamente diferente — manter unsupported mas rever se a razão deve citar a decisão D6 em vez de negar o mecanismo.

## D. Confirmações negativas úteis (NÃO são erros — parecem-no à primeira vista)

- hotplug `*.remove` (cpu/mem/disk/nic) CH+libvirt `not-implemented`: upstream suporta (CH `/vm.remove-device`), mas o adapter não tem o código → `not-implemented` correcto.
- proxmox `vm.console.serial`/`vnc` `not-implemented`: API tem `termproxy`/`vncproxy`, adapter não os chama → correcto.
- proxmox `net.vlan/vxlan/ipam/dns/routes`, firewall `logging/icmp-type` `not-implemented`: SDN/firewall PVE suporta tudo, adapter não liga (`sdn.rs:26-32` documenta-o) → `not-implemented` é a semântica exacta; candidatos a Sprint 2.
- opnsense `net.vlan/vxlan/ipv6/rate-limit/capture` `unsupported-by-provider`: o catálogo mede o tecido de rede dos *workloads* do engine, não a config administrativa da appliance → categoria diferente, correcto.
- opnsense `nat.one-to-one/multi-wan/vpn/dns/dhcp`, `firewall.icmp-type` `not-implemented`: API suporta, adapter não toca → gaps reais de Sprint 2.
- linux `net.ipv6` unsupported: recusa de segurança escrita e deliberada (`infra.rs:3645`), não confusão → correcto (mandato §10: não activar IPv6 só removendo bloqueio).
- linux `firewall.icmp-type`/`logging` not-implemented: `policy_nft.rs:73` recusa na lowering apesar de nftables suportar → ausência real do engine, `not-implemented` correcto; Sprint 2 (com limites de logging, mandato §10).

## Efeitos-cascata da aplicação em código (porque precisa de worktree + gates)

Aplicar A1–A3 toca `provider_report.rs` e `libvirt/src/lib.rs` (3 arms), e dispara:
1. **Matriz publicada**: `docs/providers/capability-matrix.md` tem de ser regenerado (`delonix provider matrix > …`) — teste `the_published_matrix_is_the_generated_one` falha senão.
2. **Ratchet de células** (`scripts/capability_baseline.json`): A1 (unsupported→not-implemented) **aumenta `applicable` em 1** (273→274) porque not-implemented conta como aplicável; A2/A3 não mexem em `applicable` (partial e not-implemented ambos aplicáveis) nem em `proved` (nenhum vira supported). É preciso `scripts/capability_ratchet.py --update` **no mesmo commit**.
3. **Contagens**: not-implemented 72→69, partial 86→88, unsupported 230→229. `proved`/supported inalterado (não há promoção falsa — coerente com §17: progresso não é só reduzir negativos).
4. **Cópia espelho** do conjunto de providers em `crates/interfaces/delonix-node-api/src/providers.rs` (teste `the_declared_providers_are_the_published_matrix`).
5. **Tradução pt.po** se alguma razão/limite novo tiver texto apresentado ao utilizador.

Conclusão: A1–A3 são uma frente de código pequena, verificável e de baixo risco — primeira a aplicar quando houver slot de WIP. B/C/D entram no Sprint 2 (implementação/validação), não aqui.
