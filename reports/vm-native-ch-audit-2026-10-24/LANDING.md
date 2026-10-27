# Landing dos dois patches (quando houver slot no WIP cap)

Ambos preparados em worktrees *detached* (não contam para o WIP cap), compilados
e verificados ao vivo. O `gitflow start` precisa de UM slot livre por patch.

## 1. fix/vm-ssh-hint-rootless  (base: main)

```sh
cd <repo>
python3 scripts/gitflow.py start fix/vm-ssh-hint-rootless      # cria worktree off main
cd ../.worktrees/<...>/vm-ssh-hint-rootless
git apply /caminho/para/reports/vm-native-ch-audit-2026-10-24/fix-vm-ssh-hint.patch
cargo build -p delonix-runtime-bin                              # verde (verificado)
# commit (Conventional), push, PR -> develop/main pela propagação GitFlow
```
Verificado: `cargo build` rc=0; `vm create` imprime
`delonix vm ssh <nome>` como passo primário e `ssh <user>@<ip>` como fallback
rotulado.

## 2. feat/vm-capability-graduation  (base: develop)

```sh
python3 scripts/gitflow.py start feat/vm-capability-graduation  # cria worktree off develop
cd ../.worktrees/<...>/vm-capability-graduation
git apply /caminho/para/reports/vm-native-ch-audit-2026-10-24/feat-vm-capability-graduation.patch
# o patch inclui o novo ficheiro scripts/vm-native-ch-lifecycle.sh
chmod +x scripts/vm-native-ch-lifecycle.sh
cargo test -p delonix-runtime-bin cmd::provider::tests::     # 4/4 verde (verificado)
```
Verificado: os dois testes-gate passam; os checks VMM-level corridos ao vivo
14/14; discovery de CH passa a **12 supported / 6 partial** em vm-compute.

## Worktrees detached (podem ser removidas após aplicar os patches)
```sh
git worktree remove --force /tmp/wt-vm-ssh-hint
git worktree remove --force /tmp/wt-vm-cap-grad
```
