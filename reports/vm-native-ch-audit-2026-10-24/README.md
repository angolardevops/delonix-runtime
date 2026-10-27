# Auditoria do caminho nativo de VMs (Cloud Hypervisor, sem libvirt) — 2026-10-24

Sprint 1 do briefing de virtualização: auditar, provar ao vivo e tornar
reproduzível o caminho nativo de VMs do delonix-runtime.

- **`AUDITORIA.md`** — componentes, independência de libvirt, falhas reproduzidas
  (topologia vs. bug), resultado do ciclo de vida, fila de correcções, estado
  dos critérios de aceitação §16.
- **`MATRIZ-CAPACIDADES.md`** — config de referência fixada + capacidades CH e
  o que o harness prova (6 `partial` graduáveis).
- **`harness-ch-lifecycle.sh`** — harness funcional reproduzível. Preflight;
  ciclo de vida completo; verifica dentro e fora do guest; destroy sem órfãos;
  limpa só o que cria; `exit != 0` em qualquer falha. Corrido: **28/28**, RC=0.
- **`evidence/`** — saída do harness, factos do host, dump bruto do discovery.

Reproduzir:  `./harness-ch-lifecycle.sh [delonix]`  (precisa de `/dev/kvm`,
`cloud-hypervisor`, `cloud-localds` e uma imagem golden cloud-init).

## Próximos passos (bloqueados nesta sessão)
Landing de código (fix do hint `ssh <ip>`; graduação de capabilities; bateria
de CI) precisa de branch própria por GitFlow — bloqueado pelo WIP cap 3/3.
Ver §7 da `AUDITORIA.md`.
