# 12 — Recuperação e GC em coexistência

SUT: `delonix 5.1.1` @ `bca34f6c3`, rootless.

| # | Caso (§19) | Observado | Estado |
|---|---|---|---|
| RECOVERY-2 | **GC com recursos activos** (`system prune` com containers vivos) | `--dry-run` separa DECLARED (0 containers/imagens a destruir sem `-a`) de DEBRIS; `prune -f` não tocou nos vivos | **PASSOU** |
| RECOVERY-2b | **GC oportunista de leaves de cgroup órfãos** (feat `startup-cgroup-gc`, commit 9d0d2dc3) | leaf órfão `dlx-relc0orphan000` criado à mão foi **reclamado** durante `run` E depois num simples `ps` (daemonless, on `open_stores`); os 3 leaves vivos (relc-keep + 2 kaeso) intactos | **PASSOU** |
| RECOVERY-1 | resiliência à morte do cliente de controlo | `run -d` com SIGKILL ao cliente a 0.4s → o supervisor destacado **completou** a criação dos 3 containers; **sem meio-estado, sem órfãos** (`prune` reclamou 0); portas correctas | **PASSOU** (propriedade de resiliência) |

## Leitura

- **O GC oportunista de leaves é real e seguro ao vivo @ HEAD:** reclama debris (leaves de cgroup
  órfãos) mesmo com recursos activos, e **não** remove o que está em uso. Valida a feature da
  memória do projecto no commit actual.
- **`prune` é seguro por omissão:** sem `-a` não remove imagens com tag (as muitas imagens do host
  marcadas `ORPHAN=yes` **não** seriam apagadas por um prune default) nem containers a correr; o
  `--dry-run` mostra as duas metades em separado (declarado vs debris) antes de agir. Honesto sobre
  limites rootless ("part of the tree was unreadable from outside the user namespace … lower bound").
- **Modelo daemonless resiliente:** matar o cliente que lançou o `run -d` não deixa meio-estado — o
  supervisor destacado conclui a operação. É uma propriedade positiva, **mas não** é o mesmo que
  interromper a meio da secção crítica.

## Limites (honestidade)

- **Não corri** o arnês de caos rigoroso do repo (`scripts/chaos.sh`, `e2e_teardown.sh`,
  `net-lifecycle-leaks.sh`) que faz SIGINT/SIGKILL a meio de cada mutação e conta veth/bridge/nft/mount
  órfãos sistematicamente. O meu RECOVERY-1 provou resiliência à morte do cliente, não a interrupção
  mid-mutation. → a cobertura rigorosa de §19 continua a assentar nesse arnês (candidato a correr numa
  próxima ronda).
- Não forcei falha de escrita em volume limitado, reboot, nem indisponibilidade remota (providers).

## Preservação

Todos os recursos `relc-*` removidos; `kaeso-odoo`/`kaeso-db` intactos em todo o percurso; nenhuma
imagem/rede pré-existente apagada; disco não cheio (mantido ~96%, nunca empurrado ao limite).
