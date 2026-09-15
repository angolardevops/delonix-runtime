# Rede e CNI — `fa8c6297`

Fontes: bateria E2E (`raw/e2e.log`), caos (`raw/chaos.log`), injecção de falha G-008, critest.

| Área | Evidência | Estado |
|---|---|---|
| Bridge/veth, IP por container, attach concorrente (8) e escala (30), IPs distintos | caos `concurrent-attach`, `scale` | PASS |
| Isolamento de namespace containers e pods | caos `namespace-isolation`, `pod-namespace-isolation` | PASS |
| Holder: kill do pin, morte total, restart do control, wedge, slirp kill, idempotência | caos (6 cenários) | PASS |
| Posse entre roots (pid reciclado de outro root) | caos `posse-destrutiva/*` | PASS |
| NetworkRoute prune/destroy sem órfãos | caos `stack-netroute` | PASS |
| Port publishing, ingress/egress, DNS interno | E2E | PASS |
| **Anti-spoof falha aberto** quando o insert nft falha | injecção com shim, controlo com regra | **FAIL (G-008)** |
| Port mapping/multi-container vistos do host (critest) | IP do pod no netns do holder, inalcançável do host | ROOTLESS LIMITATION |
| Port-forward CRI (2 specs) | timeout; sem log de causa | UNKNOWN (G-031) |
| CNI ADD/DEL/CHECK com plugins externos (`DELONIX_CNI=1`) | — | NOT TESTED |
| IPv6 / dual-stack | desligado por desenho (fail-closed, v0.37.1) | DELIBERATE DIVERGENCE |
| WireGuard/overlay inter-nó | exige 2 nós | ENVIRONMENT BLOCKED |
| Reaper de forwards do PaaS com host address / sem registo | leitura de código `delonix-paas` origin/main | FAIL (G-036, G-037) |
| Restart do host | — | NOT TESTED |
