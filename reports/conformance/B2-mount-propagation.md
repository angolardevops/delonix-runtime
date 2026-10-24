# B2 — Mount propagation (Track B · também fecha 3 falhas do CRI)

**Data:** 2026-10-24 · **Binário:** `delonix 5.1.1 @ ee615f3b` · rootless.

## Resultado: JÁ IMPLEMENTADO e verificado efectivo ao nível do motor

Os documentos datados (`paridade-docker-podman.md` "só `:ro`" e `cri-conformance.md` "3 falhas de
mount-propagation") são de **v0.63.1** e estão **stale**. No binário atual a propagação funciona.

### Verificação ao vivo (mountinfo do container)

| Opção | Esperado | Observado | Estado |
|---|---|---|---|
| `-v H:/mnt:rslave` | tag `master:` | `… /mnt rw,nosuid,nodev,relatime master:1` | **PASSOU** |
| `-v H:/mnt:rshared` | tag `shared:` | `… /mnt … shared:1187 master:1` | **PASSOU** |
| `-v H:/mnt:rprivate` | sem tags | `… /mnt rw,nosuid,nodev,relatime` (sem shared/master) | **PASSOU** |
| `-v H:/mnt:zzz` (inválido) | erro explícito | `error[DX-1210] unsupported bind option ':zzz' — supported: …` | **PASSOU** (fail-closed) |

### Código (atual)

- `delonix-volume/src/lib.rs:1075-1083` parseia `rprivate`/`rslave`/`rshared` **e** os combos `ro,<prop>`.
- `delonix-linux/src/lib.rs:1439-1455` aplica `MS_SLAVE`/`MS_SHARED`/`MS_PRIVATE` com `MS_REC` como um
  `mount()` próprio (ordem correcta por regra do kernel — a propagação em chamada separada).
- `delonix-cri/.../lifecycle.rs:3052-3056` traduz o CRI: `PropagationHostToContainer→rslave`,
  `Bidirectional→rshared`, default→private (direcção segura).

## CRI (os 3 specs): confirmação ADIADA — colisão com sessão concorrente

A confirmação via `critest` (os 3 specs de "Container Mount Propagation") **não foi corrida aqui**:
o host tem uma **sessão concorrente do dono** num worktree `cri-image-repodigests-and-user` com
`delonix-cri` + `critest` activos (partilham `/tmp/delonix-cri-critest-root.*` e socket). Um
`critest FOCUS=propagation` daqui colidiu (timeout aos 300 s no setup) e arriscava interferir com a
corrida dele. **Não se matou nenhum processo do dono.** Como o motor aplica a propagação (prova
acima) e o caminho CRI traduz para a MESMA propagação, os 3 specs devem passar — mas o número fica
para a **corrida de critest do Track C**, coordenada, sem colisão.

> Nota: o worktree do dono (`cri-image-repodigests-and-user`) ataca precisamente o **C2** do plano
> (Image Manager: repodigests + Uid/Username) — Track C já está parcialmente em curso por ele.

## Veredicto B2

- **Feito ao nível do motor**, verificado ao vivo. A dependência do **C4** (mount-propagation) do
  Track C está satisfeita.
- Pendente: o número do critest para os 3 specs, a obter numa corrida coordenada (o dono está a
  correr critest agora).
