# Storage e volumes sob falha — medido sobre `fa8c6297`

Estado isolado (`/tmp/dxs`), 2026-09-15. Log: `raw/storage-failure.txt`.

| Cenário | Resultado | Estado |
|---|---|---|
| `volume rm` com container a usar o volume | recusado («in use by container(s): c1 (running)»), dados intactos | PASS |
| `backup create container c1` | arquivo 1,2 KiB com `volumes/v1` | PASS |
| `backup restore` sobre container a correr sem `--force` | recusado com razão | PASS |
| `backup inspect` de arquivo truncado a 50% | **rc=0**, lista volumes | PARTIAL (G-038) |
| `backup restore --force` de arquivo truncado | rc=1 `failed to unpack …container.json`; **PID igual** (não reiniciou); dados actuais intactos | PASS (fail-closed) |
| Restore válido depois de `stop` | `volume v1 restored`, dado original de volta | PASS |
| Temporários de restore após falha | `/tmp/dxs/tmp` vazio | PASS |
| Disk full (caos) | só o padrão tmp→fsync→rename; esgotamento real saltado | PARTIAL |
| Write failure (caos) | rc=77, zero temporários | PASS |
| Destroy de stack parcialmente aplicado (caos) | leva o órfão, poupa o alheio | PASS |
| Snapshots de volume, quotas, sparse | — | NOT TESTED |
| NFS/CIFS/TrueNAS | exige CAP_SYS_ADMIN / appliance | ENVIRONMENT BLOCKED |
| Interrupção a meio do backup (SIGKILL) | — | NOT TESTED |
| Symlink malicioso no volume durante backup | — | NOT TESTED |

Nenhuma operação irreversível foi observada a apresentar-se como transaccional: o restore falhado
não mexeu no estado, e o `--force` só pára o recurso quando o arquivo é legível.
