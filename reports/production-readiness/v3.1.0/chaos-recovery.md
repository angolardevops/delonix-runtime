# Caos e recuperação — medido sobre `fa8c6297`

`scripts/chaos.sh --bin <fa8c6297 debug>`, sandbox `/tmp/dlx-chaos` (root e runtime dir próprios),
load 1m 2,93 / 32 threads, 2026-09-15, 177 s. Log: `raw/chaos.log`.

**29 PASS · 0 FAIL · 1 SKIP**

| Área | Cenários | Resultado |
|---|---|---|
| Holder / plano de controlo | holder-kill, full-holder-death, control-restart, holder-wedge, slirp-kill, idempotent-up | PASS — pin morre sem custar PIDs de workload; morte total reconstrói e reporta |
| Posse entre roots | pin, teardown, cross-root, proxy-cross-root | PASS — pid reciclado de outro root não é lido nem morto |
| Recursos | oom, concurrent-attach (8), scale (30), scale-cleanup (0 MiB), abrupt-kill, aggregate-ceiling, delegated-scope, cgroup-netns | PASS |
| Isolamento | namespace-isolation, pod-namespace-isolation, pod-holder-respawn | PASS |
| Durabilidade | disk-full, write-failure (rc=77) | PASS **parcial**: o esgotamento real de disco foi saltado (90,5 GiB livres) |
| Estado desejado | stack-converge ×3, stack-netroute ×2, stack-partial-apply | PASS |
| Storage remoto | truenas-destroy | **SKIP** — sem credenciais |

## Não exercitado

- Esgotamento real de disco (precisa de um filesystem pequeno).
- `truenas-destroy`.
- Reboot do host, rootful, upgrade in-place com holder antigo, SELinux.

## Nota

Na CI este mesmo arnês foi **skipped** para este commit (`chaos.yml` run 34988281401). Este é o
primeiro resultado de caos sobre `fa8c6297`.
