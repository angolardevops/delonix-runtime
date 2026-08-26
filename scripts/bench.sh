#!/usr/bin/env bash
# Bancada de latência: Delonix × Docker × Podman, na mesma máquina, no mesmo dia.
#
# ## O que este script existe para impedir
#
# A bateria de 2026-08-10 deste projecto foi RETIRADA e a razão está escrita no
# `docs/comparacao-medida.md`: dava docker 1406 ms, podman 1351 ms, delonix
# 640 ms, e a corrida seguinte — nas mesmas ferramentas, mesma distro, mesmo
# kernel — deu 208 / 268 / 89. Três motores a acelerarem seis vezes ao mesmo
# tempo não é melhoria de nenhum: é a bancada. Aqueles números mediam a
# contenção da máquina naquele instante.
#
# Por isso este script **mede a bancada antes de medir as ferramentas, e
# RECUSA-SE a correr quando ela não serve**. É a lição transformada em código,
# em vez de uma nota que a próxima pessoa lê depois de já ter publicado.
#
# ## O que publica
#
# Hardware, kernel, versões das três ferramentas, carga, densidade do nó, e por
# cada linha a MEDIANA com as amostras todas — não só o número bonito. Uma
# mediana sem dispersão esconde o caso em que metade das corridas foi o dobro.
#
# ## Uso
#
#   scripts/bench.sh [--runs N] [--force] [--json] [--max-load N]
#
# `--force` corre numa bancada recusada e marca o resultado como NÃO PUBLICÁVEL.
# Serve para depurar o próprio script, não para produzir uma tabela.

set -uo pipefail

RUNS=10
FORCE=0
JSON=0
MAXLOAD=""
BIN="${BIN:-$(cd "$(dirname "$0")/.." && pwd)/target/release/delonix}"
while [[ $# -gt 0 ]]; do
  case "$1" in
    --runs) RUNS="$2"; shift 2 ;;
    --force) FORCE=1; shift ;;
    --json) JSON=1; shift ;;
    --bin) BIN="$2"; shift 2 ;;
    # O limiar por omissão é metade dos threads. Quem corre numa máquina
    # DEDICADA quer ser mais estrito do que isso — ali um load de 1 já é
    # alguém a fazer login. Também é o que torna a recusa testável sem
    # depender da carga real da máquina onde o teste corre.
    --max-load) MAXLOAD="$2"; shift 2 ;;
    *) echo "uso: $0 [--runs N] [--force] [--json] [--bin PATH] [--max-load N]"; exit 2 ;;
  esac
done

# Um binário que não existe reporta-se como AUSENTE, nunca como uma linha vazia.
# A primeira corrida deste script imprimiu `delonix:` sem versão e
# `densidade: 0` — os dois vindos de um binário inexistente, e os dois a ler-se
# como medições. Um harness que mede o que não está lá é o relato desonesto que
# ele próprio existe para impedir.
if [[ ! -x "$BIN" ]]; then
  echo "ERRO: binário do delonix não encontrado em $BIN" >&2
  echo "      constrói com \`cargo build --release -p delonix-runtime-bin\` ou passa --bin" >&2
  exit 2
fi

NCPU=$(nproc)
LOAD1=$(awk '{print $1}' /proc/loadavg)
KERNEL=$(uname -r)
CPU=$(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2 | sed 's/^ *//')
MEM=$(awk '/MemTotal/{printf "%.0f GiB", $2/1048576}' /proc/meminfo)

# A densidade do nó é PARTE da bancada e não um detalhe: duas chamadas do
# caminho de attach (`nft -a list chain fwdeny`, `nft list sets`) são dumps de
# texto que crescem com o número de containers. O `comparacao-medida.md` marca
# isto como por medir e diz que o nó da altura estava VAZIO — comparar com uma
# corrida num nó cheio compara duas perguntas diferentes.
DENSITY=$("$BIN" container ls 2>/dev/null | tail -n +2 | wc -l | tr -d ' ')

# O limiar: metade dos CPUs. Não é um número mágico — é onde a fila de execução
# começa a somar-se a cada medição, e o efeito é multiplicativo, não aditivo.
THRESHOLD="${MAXLOAD:-$(awk -v n="$NCPU" 'BEGIN{printf "%.2f", n/2}')}"
BANCADA_OK=$(awk -v l="$LOAD1" -v t="$THRESHOLD" 'BEGIN{print (l<t)?1:0}')

echo "== bancada =="
echo "  cpu:        $CPU ($NCPU threads)"
echo "  memória:    $MEM"
echo "  kernel:     $KERNEL"
echo "  load(1m):   $LOAD1   (limiar: $THRESHOLD${MAXLOAD:+ — via --max-load})"
echo "  densidade:  $DENSITY container(s) delonix a correr"
for t in docker podman; do
  if command -v "$t" >/dev/null; then
    echo "  $t: $("$t" --version 2>/dev/null | head -1)"
  else
    echo "  $t: AUSENTE — a coluna dele fica 'não medido', nunca inventada"
  fi
done
echo "  delonix:    $("$BIN" --version 2>/dev/null | head -1)"
echo

if [[ "$BANCADA_OK" != "1" ]]; then
  echo "RECUSADO: load $LOAD1 acima do limiar $THRESHOLD."
  echo
  echo "  Uma medição aqui mede a CONTENÇÃO desta máquina, não as ferramentas."
  echo "  Foi assim que a bateria de 2026-08-10 acabou retirada: três motores"
  echo "  seis vezes mais lentos ao mesmo tempo não é uma propriedade de nenhum."
  echo
  echo "  Para medir a sério: uma máquina ociosa e dedicada (a bateria publicada"
  echo "  usou uma VM criada com o próprio motor), ou esperar que a carga desça."
  [[ "$FORCE" != "1" ]] && exit 3
  echo "  --force: a correr na mesma. O RESULTADO NÃO É PUBLICÁVEL."
  echo
fi

# Mediana + amostras. A mediana sozinha esconde metade da história — se cinco de
# dez corridas forem o dobro, a mediana não muda e a ferramenta é outra coisa.
stats() {
  local -a v=("$@")
  local n=${#v[@]}
  local sorted
  sorted=$(printf '%s\n' "${v[@]}" | sort -n)
  local med
  med=$(printf '%s\n' "$sorted" | awk -v n="$n" '{a[NR]=$1} END{print (n%2)?a[(n+1)/2]:int((a[n/2]+a[n/2+1])/2)}')
  local min max
  min=$(printf '%s\n' "$sorted" | head -1)
  max=$(printf '%s\n' "$sorted" | tail -1)
  echo "$med|$min|$max|$(printf '%s ' "${v[@]}")"
}

time_n() {
  local -a samples=()
  local s e
  for _ in $(seq "$RUNS"); do
    s=$(date +%s%N)
    "$@" >/dev/null 2>&1
    e=$(date +%s%N)
    samples+=( $(( (e - s) / 1000000 )) )
  done
  stats "${samples[@]}"
}

row() {
  local label="$1"; shift
  if ! command -v "$1" >/dev/null 2>&1 && [[ ! -x "$1" ]]; then
    printf '  %-28s %s\n' "$label" "não medido (ferramenta ausente)"
    return
  fi
  local r; r=$(time_n "$@")
  printf '  %-28s %6s ms   (min %s, max %s)\n' "$label" \
    "$(cut -d'|' -f1 <<<"$r")" "$(cut -d'|' -f2 <<<"$r")" "$(cut -d'|' -f3 <<<"$r")"
  printf '  %-28s %s\n' "" "amostras: $(cut -d'|' -f4 <<<"$r")"
}

# Root isolado para o delonix: esta máquina pode ter carga real a correr, e a
# regra do repo é isolar os DOIS caminhos — só o `DELONIX_ROOT` põe dois roots a
# disputar os mesmos sockets (ver AGENTS.md, «Meia-isolação é pior que nenhuma»).
SANDBOX=$(mktemp -d /tmp/dlx-bench.XXXXXX)
trap 'rm -rf "$SANDBOX"' EXIT
mkdir -p "$SANDBOX/root" "$SANDBOX/run"
dlx() { env DELONIX_ROOT="$SANDBOX/root" DELONIX_NET_RUNTIME_DIR="$SANDBOX/run" "$BIN" "$@"; }
export -f dlx 2>/dev/null || true

# AQUECIMENTO, e não é cosmética: o root do delonix é isolado e nasce VAZIO, por
# isso a primeira corrida paga o pull e a extracção da imagem — enquanto o docker
# e o podman a têm local. Medido: 6 275 / 7 408 / 7 359 ms num root virgem contra
# 88 ms a seguir, e uma amostra de 16 782 ms na primeira versão desta bancada.
#
# A mediana absorveu-o, o que é precisamente o perigo: com `--runs 1` o número
# publicado seria dezasseis segundos, e a média teria ficado destruída sem nada
# na tabela a dizer porquê. Comparar um motor a frio com dois a quente não é a
# comparação que esta linha diz fazer.
echo "== aquecimento (o root do delonix nasce vazio; docker e podman já têm a imagem) =="
env DELONIX_ROOT="$SANDBOX/root" DELONIX_NET_RUNTIME_DIR="$SANDBOX/run" \
  "$BIN" container run --rm alpine true >/dev/null 2>&1
docker run --rm alpine true >/dev/null 2>&1
podman run --rm alpine true >/dev/null 2>&1
echo

echo "== 4a: latência de \`run --rm\`, no DEFAULT de cada motor (n=$RUNS) =="
row "docker (bridge)" docker run --rm alpine true
row "podman (slirp)"  podman run --rm alpine true
row "delonix (host)"  env DELONIX_ROOT="$SANDBOX/root" DELONIX_NET_RUNTIME_DIR="$SANDBOX/run" \
    "$BIN" container run --rm alpine true
echo
echo "Nota: os defaults NÃO são a mesma configuração. O do delonix é \`--net host\`;"
echo "para a comparação com rede isolada ver o \`docs/comparacao-medida.md\`, linha 4b."
