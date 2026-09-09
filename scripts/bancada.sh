#!/usr/bin/env bash
# A BANCADA — uma única opinião sobre «esta máquina serve para medir?».
#
# Não se corre: sourceia-se.
#
#   . "$(dirname "$0")/bancada.sh"
#   bancada_medir
#   [[ "$BANCADA_OK" == 1 ]] || bancada_recusa "chaos.sh" "<consequência>"
#
# ## Porque existe, e porque é PARTILHADA
#
# A bateria de latência de 2026-08-10 foi RETIRADA e a razão está no
# `docs/comparacao-medida.md`: dava docker 1406 ms, podman 1351 ms, delonix
# 640 ms, e a corrida seguinte — mesmas ferramentas, mesma distro, mesmo kernel —
# deu 208 / 268 / 89. Três motores a acelerarem seis vezes ao mesmo tempo não é
# melhoria de nenhum: é a bancada. O `bench.sh` transformou essa lição em código
# e passou a RECUSAR-SE a correr numa máquina carregada.
#
# O `chaos.sh` precisava exactamente do mesmo juízo e não o tinha. Medido
# 2026-09-09, com carga concorrente na máquina: `FAIL scale — só 0 de 30
# containers ganharam IP`, e SKIP em `abrupt_kill`, `cgroup_netns` e
# `stack_netroute` («container não arrancou»). Repetido com a máquina mais
# quieta, sem mudar uma linha: `scale` 30/30 PASS e os outros três PASS.
#
# (O `stack_partial_apply`, que saltava ao lado destes, NÃO era da bancada: era
# a grafia `volumes` — removida no B2 — dentro do próprio cenário. Corrigido.
# Vale a pena separar as duas causas: uma resolve-se com um portão, a outra com
# uma correcção, e confundi-las adiava as duas.)
#
# Um FAIL que se lê como defeito do produto e é da bancada é a pior espécie de
# ruído num portão — pior do que não ter portão, porque gasta o crédito de quem
# o lê.
#
# Está aqui, e não copiado nos dois, porque duas cópias divergem: a segunda
# ganha um limiar «só um pouco mais permissivo» na tarde em que alguém tem
# pressa, e a partir daí os dois harnesses respondem coisas diferentes à mesma
# pergunta sobre a mesma máquina.
#
# ## O limiar
#
# Uma FRACÇÃO dos threads, e a fracção é de quem chama — porque as duas
# perguntas não têm a mesma sensibilidade, e isso está medido.
#
#   `bench.sh`  metade  (`nproc / 2`). Mede latência de arranque: a fila de
#               execução soma-se a cada amostra, mas soma-se gradualmente.
#   `chaos.sh`  um quarto (`nproc / 4`). Arranca TRINTA containers em paralelo,
#               e ali a degradação não é gradual — é um precipício.
#
# O quarto não é um palpite. Medido 2026-09-09 neste host de 32 threads (com
# java, rustc, postgres e qemu reais a correr ao lado), o MESMO script e o MESMO
# binário:
#
#   load 11.56  ->  `scale` 0/30 containers com IP. E não recuperou: os três
#                   cenários seguintes que precisam da rede custom
#                   (`abrupt_kill`, `cgroup_netns`, `stack_netroute`) saltaram
#                   com «container não arrancou».
#   load  5.47  ->  `scale` 30/30, e os mesmos três a PASSAR (6 checks verdes).
#
# 11.56 passava por baixo do limiar do `bench.sh` (16.00) sem ser notado, o que
# é precisamente o falso-defeito que este portão existe para tirar do relatório.
# O quarto (8.00) cai dentro do intervalo medido. É provisório com dois pontos:
# quem estreitar o intervalo deve baixá-lo, não subi-lo.
#
# Quem corre numa máquina DEDICADA quer ser mais estrito ainda (ali um load de 1
# já é alguém a fazer login) — é isso que o `--max-load` serve, e é também o que
# torna a própria recusa testável sem depender da carga real do host.

# Mede a bancada. `$1` (opcional) é um limiar explícito — o `--max-load` de quem
# chama, e ganha a tudo. `$2` (opcional, default 2) é o divisor de `nproc` que
# dá o limiar por omissão. Exporta BANCADA_NCPU, BANCADA_LOAD1,
# BANCADA_THRESHOLD e BANCADA_OK (1 = serve).
bancada_medir() {
  local maxload="${1:-}" divisor="${2:-2}"
  BANCADA_NCPU=$(nproc)
  BANCADA_LOAD1=$(awk '{print $1}' /proc/loadavg)
  BANCADA_THRESHOLD="${maxload:-$(awk -v n="$BANCADA_NCPU" -v d="$divisor" 'BEGIN{printf "%.2f", n/d}')}"
  BANCADA_OK=$(awk -v l="$BANCADA_LOAD1" -v t="$BANCADA_THRESHOLD" 'BEGIN{print (l<t)?1:0}')
}

# Imprime a recusa e sai com 3. `$1` é o nome do harness, `$2` a consequência
# concreta de correr assim — o que o RELATÓRIO dele vai dizer de errado. A
# consequência é por harness porque é a única parte que difere: no `bench.sh` a
# mentira é um número, no `chaos.sh` é um veredicto.
bancada_recusa() {
  local harness="$1" consequencia="$2"
  echo "RECUSADO: load $BANCADA_LOAD1 acima do limiar $BANCADA_THRESHOLD."
  echo
  echo "  $consequencia"
  echo
  echo "  Foi assim que a bateria de 2026-08-10 acabou retirada: três motores"
  echo "  seis vezes mais lentos ao mesmo tempo não é uma propriedade de nenhum."
  echo
  echo "  Para correr a sério: uma máquina ociosa e dedicada (a bateria publicada"
  echo "  usou uma VM criada com o próprio motor), ou esperar que a carga desça."
  echo "  \`--force\` corre na mesma e marca o resultado como NÃO PUBLICÁVEL;"
  echo "  \`--max-load N\` muda o limiar ($harness)."
  exit 3
}
