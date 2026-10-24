# 05 — Rede, scopes/cgroups e isolamento

SUT: `delonix 5.1.1` @ `bca34f6c3`, **rootless**. Rede de teste: `relc-net` (`dlxn93807333`, 10.206.0.0/16).

| # | Caso | Verificação independente | Observado | Estado |
|---|---|---|---|---|
| NETWORK-1 | topologia | `ip`, `network ls` | rede criada, bridge `dlxn93807333`, subnet /16; bridge vive **dentro do netns rootless** (slirp4netns), invisível no root netns do host — esperado em rootless | **PASSOU** |
| NETWORK-2 | porta publicada + HTTP externo | `curl` + log nginx | `HTTP 200`; pedido visto no log vindo de `10.0.2.2` (slirp gw) | **PASSOU** (ver 03) |
| NETWORK-3 | **republicação de porta em execução** | `describe` PID + `curl` | PID `1098119` **estável** em `--publish-add 18082` (novo→200, velho→200) e `--publish-rm 18081` (velho→000, novo→200). **Processo nunca reiniciado.** | **PASSOU** |
| NETWORK-4a | DNS por nome | `getent`, wget | `relc-a` resolve `relc-b`→10.206.61.43; alpine wget obtém página do nginx por nome | **PASSOU** |
| NETWORK-4b | isolamento entre namespaces | wget por nome **e por IP** | teamA↔teamA reachable (controlo); teamB→teamA **bloqueado por nome E por IP directo** (nft drop, não só DNS) | **PASSOU** |
| SCOPE-1 | cgroups v2 delegate efectivos | `/sys/fs/cgroup` + `system resources` | `memory.max`/`cpu.max` efectivos (ver 03); `--cpuset` aceite-e-ignorado (DLX-RES-001), io recusado exit 69 (DLX-RES-002) — motor declara o que não aplica | **PASSOU** |
| SCOPE-2 | mesmo NOME em namespaces diferentes | `ps`, `system namespace ls` | `relc-dup` coexiste em teamA e teamB; `namespace ls` conta 1+1; resolução correcta | **PASSOU** |

## Notas de design confirmadas no código

- **`default` é o namespace partilhado por desenho** (`firewall.rs:780-789,3172-3199`): a
  isolação só é imposta **entre namespaces não-`default`**. Um primeiro teste (teamB→`default`)
  deu "reached"; depois de ler o código, re-testei o par correcto (teamA↔teamB) — bloqueado.
  *Registado para evitar que seja lido como defeito: não é.*
- **Isolamento robusto:** o bloqueio teamB→teamA aguenta mesmo **por IP directo**, logo não
  depende de DNS scoping — é `nft` a largar a ligação nova (coerente com `network.rs:81`
  "same-namespace accept, cross-namespace new connections dropped").
- **Conflito correcto:** `network rm` de uma rede em uso → `error[DX-5307] conflict ... in use by container` (exit 5).

## Modelo daemonless e "restart do engine"

O requisito §8 "repete após restart do engine para verificar persistência" é **N/A no sentido
Docker**: Delonix é **daemonless** — não há engine a reiniciar. O dataplane (nft DNAT + hostfwd
sobre o slirp) persiste independentemente do processo que o criou; a persistência relevante
(sobreviver à saída do processo de controlo) foi provada em 03 (stop/start, republish).

## Limpeza

Todos os containers `relc-*` e `relc-net` removidos; namespaces teamA/teamB desapareceram ao
ficarem vazios (comportamento documentado). `kaeso-*` preservados.
