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
# O `chaos.sh` precisava do mesmo juízo e não o tinha: um FAIL que se lê como
# defeito do produto e é da bancada é a pior espécie de ruído num portão — pior
# do que não ter portão, porque gasta o crédito de quem o lê.
#
# DUAS notas de honestidade sobre como este portão apareceu, porque as duas vezes
# a bancada foi acusada de coisas que não eram dela:
#
#   * o `stack_partial_apply`, que saltava junto com os outros, era a grafia
#     `volumes` — removida no B2 — dentro do próprio cenário. Corrigido.
#   * o `scale` 0/30, que foi o caso que MOTIVOU este ficheiro, é o
#     `pod_holder_respawn` a deixar o sandbox meio-de-pé (ACH-015). Medido e
#     isolado — ver a secção do limiar abaixo.
#
# Separar as causas importa: uma resolve-se com um portão, as outras com
# correcções, e confundi-las adiava as três.
#
# Está aqui, e não copiado nos dois, porque duas cópias divergem: a segunda
# ganha um limiar «só um pouco mais permissivo» na tarde em que alguém tem
# pressa, e a partir daí os dois harnesses respondem coisas diferentes à mesma
# pergunta sobre a mesma máquina.
#
# ## O limiar
#
# Metade dos threads, para os DOIS harnesses. Não é um número mágico — é onde a
# fila de execução começa a somar-se a cada medição, e o efeito é multiplicativo,
# não aditivo. Quem corre numa máquina DEDICADA quer ser mais estrito do que
# isso (ali um load de 1 já é alguém a fazer login), e é isso que o `--max-load`
# serve; é também o que torna a própria recusa testável sem depender da carga
# real do host onde o teste corre.
#
# ## O quarto que aqui esteve, e porque saiu (2026-09-09)
#
# Este ficheiro afirmou durante algumas horas que o `chaos.sh` precisava de um
# limiar mais estrito (`nproc / 4`), com dois pontos por trás:
#
#   load 11.56  ->  `scale` 0/30 containers com IP
#   load  5.47  ->  `scale` 30/30
#
# O par estava CONFUNDIDO, e a terceira medição desfez-o: a suite completa numa
# máquina quieta deu `scale` 0/30 a **load 5.49** — praticamente a mesma carga a
# que tinha passado. A diferença entre as duas corridas nunca foi o load: a que
# passou era um SUBCONJUNTO de cenários que não incluía o `pod_holder_respawn`, e
# a que chumbou corria-o imediatamente antes.
#
# Isolado, ao lado, com a máquina quieta nas duas:
#
#   `scale` sozinho, load 3.65                       ->  30/30 PASS
#   `pod_holder_respawn` + `scale`, load 4.30        ->  0/30 FAIL
#
# A causa é o cenário anterior deixar o sandbox meio-de-pé (ver ACH-015 e o
# comentário no `scen_pod_holder_respawn`), não a bancada. O quarto voltou a ser
# metade porque não há UMA medição que o justifique — e um limiar mais estrito do
# que o medido é a mesma afirmação-sem-prova que este ficheiro existe para
# impedir, só virada para o lado prudente.
#
# O portão FICA. Não porque o `scale` o exija, mas porque a razão original dele é
# independente e continua de pé: um veredicto de arnês colhido numa máquina
# carregada não distingue defeito de contenção, e é indistinguível de um verde
# para quem o lê.

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
