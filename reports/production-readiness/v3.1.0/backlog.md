# Backlog priorizado — saído da auditoria de `fa8c6297`

Cada item aponta para a linha da `gap-matrix.csv`. Nada aqui foi implementado.

## P0
Nenhum confirmado. **Reproduzir G-007** (forçar falha do mount tmpfs em teste de integração) — se
confirmar escrita de segredos no upper, sobe a P0.

## P1 — bloqueios de produção
1. **G-008** anti-spoof fail-closed: `run_ok` → `run` com erro propagado nos três sítios; attach falha
   se a regra não entrar. Teste: o shim nft desta auditoria (`raw/g008-nft-shim.sh`) como teste de
   integração. Exige respawn do holder no rollout.
2. **G-007 / G-026** `write_secret_files` fail-closed: segredos pedidos e não entregues → `run` falha.
3. **G-029** Docker API: reconciliar estado antes de responder a `ps`/`inspect`.
4. **G-027** `docker run -d` pendura — investigar o fluxo create+start+attach do CLI Go.
5. **G-003** job de caos que falha em vez de `skipped` quando não pode correr.
6. **G-036** (delonix-paas) usar `delonix_net::parse_publish` no reaper.
7. **G-005** CRI: pull por digest e `Username` no image status (bugs); RunAsUserName; OOMKilled
   (verificar `4a4d029f` sobre a release candidata).
8. **G-009** masked/readonly paths e rlimits pedidos explicitamente → fail-closed.
9. **G-024** bateria rootful numa VM descartável.

## P2
- G-011 Compose sub-chaves aninhadas desconhecidas → recusa.
- G-033 tag+digest em conflito → recusa.
- G-037 (paas) forwards sem registo de container fora do reaper.
- G-010 rro via `mount_setattr(AT_RECURSIVE)`.
- G-012 Docker API: campos ignorados sem aviso.
- G-020 pinar actions por SHA.
- G-021 artefactos aarch64; verificar assinatura/provenance dos assets.
- G-025 docs de conformidade regeneradas (um número, versão, data, comando).
- G-028 `docker run --rm` sem órfãos.
- G-030 `critest.sh` com os dois roots isolados e cleanup correcto.
- G-031 log de erros gRPC no `delonix-cri`.
- G-041 pause→Stopped (já em origin/main — confirmar na release).
- G-002 testes live com skip explícito em vez de pass silencioso.

## P3
G-014, G-015, G-016, G-017, G-018, G-034, G-035, G-038, G-039, G-042, G-004 (limiar do check), G-006, G-022.
