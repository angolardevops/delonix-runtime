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
#
# ## `--json`
#
# **Esta flag era aceite e IGNORADA** — `JSON=1` era atribuído e nunca lido, por
# isso `--json` imprimia a mesma tabela para humanos que uma corrida normal.
# Pertence à classe que este repositório persegue em todo o lado: uma opção que o
# utilizador passou e o programa engoliu em silêncio (ver as três já corrigidas
# no `AGENTS.md` — `--security-opt seccomp=`, `-v …:z`, `--network-alias`).
#
# Agora emite UM objecto JSON em **stdout** e manda todo o texto humano para
# **stderr**, o mesmo contrato do `-o json` do resto da CLI: quem faz
# `bench.sh --json > run.json` fica com um ficheiro que parseia, e continua a ver
# o relatório no terminal. É o que o `scripts/bench_gate.py` consome — o gate LÊ
# uma medição, nunca a faz, para nunca haver duas opiniões sobre o mesmo número.
#
# O objecto diz SEMPRE se é publicável (`bench.publishable`), e uma ferramenta
# ausente sai `null` e nunca `0`: um zero lê-se como medição.

set -uo pipefail

# O juízo sobre a bancada é PARTILHADO com o `chaos.sh` — ver
# `scripts/bancada.sh`. Duas cópias divergem; uma só responde a mesma
# pergunta a quem quer que a faça.
# shellcheck source=scripts/bancada.sh
. "$(cd "$(dirname "$0")" && pwd)/bancada.sh"

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
# O stdout REAL fica no fd 3 e o resto do script escreve para stderr quando
# `--json` está ligado. Uma linha de relatório no meio do objecto tornava a
# saída impossível de parsear, e mandar o relatório para o limbo tornava a flag
# inútil num terminal — assim as duas metades sobrevivem.
if [[ "$JSON" == "1" ]]; then
  exec 3>&1 1>&2
else
  exec 3>&1
fi

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

# O limiar e o veredicto vêm do helper partilhado (`bancada_medir`), não de uma
# segunda opinião escrita aqui.
bancada_medir "$MAXLOAD"
THRESHOLD="$BANCADA_THRESHOLD"

echo "== bancada =="
echo "  cpu:        $CPU ($NCPU threads)"
echo "  memória:    $MEM"
echo "  kernel:     $KERNEL"
echo "  load(1m):   $LOAD1   (limiar: $THRESHOLD${MAXLOAD:+ — via --max-load})"
echo "  densidade:  $DENSITY container(s) delonix a correr"
declare -A TOOLVER=()
for t in docker podman; do
  if command -v "$t" >/dev/null; then
    TOOLVER[$t]=$("$t" --version 2>/dev/null | head -1)
    echo "  $t: ${TOOLVER[$t]}"
  else
    echo "  $t: AUSENTE — a coluna dele fica 'não medido', nunca inventada"
  fi
done
TOOLVER[delonix]=$("$BIN" --version 2>/dev/null | head -1)
echo "  delonix:    ${TOOLVER[delonix]}"
echo

if [[ "$BANCADA_OK" != "1" ]]; then
  if [[ "$FORCE" != "1" ]]; then
    bancada_recusa "bench.sh" \
      "Uma medição aqui mede a CONTENÇÃO desta máquina, não as ferramentas."
  fi
  echo "AVISO: load $LOAD1 acima do limiar $THRESHOLD, e --force pediu para correr."
  echo "  O RESULTADO NÃO É PUBLICÁVEL."
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
  local min max spread
  min=$(printf '%s\n' "$sorted" | head -1)
  max=$(printf '%s\n' "$sorted" | tail -1)
  # max/min. A mediana sozinha esconde metade da história — foi por isso que
  # este script sempre publicou min e max —, mas ninguém JULGAVA esses dois
  # números. Medido a 2026-09-23, com o load em 6.44 (bem abaixo do limiar): o
  # docker deu 434 ms de mínimo e 5 407 de máximo na mesma corrida de dez, e a
  # mediana engoliu-o. Uma baseline gravada assim fica envenenada, e o limiar de
  # load não a apanha — a contenção que dispersa I/O não aparece no load(1m).
  spread=$(awk -v a="$min" -v b="$max" 'BEGIN{printf "%.2f", (a>0)? b/a : 0}')
  echo "$med|$min|$max|$(printf '%s ' "${v[@]}")|$spread"
}

# O exit status de cada corrida CONTA. Sem isto, uma ferramenta instalada mas
# inutilizável — o caso mais comum é o `docker` presente com o daemon parado —
# falha em dezenas de milissegundos e entra na tabela como a mais rápida das
# três. É a mesma falha que o cabeçalho deste ficheiro já impede para um binário
# do delonix ausente (`densidade: 0` lido como medição), só que a um comando de
# distância: um harness que mede o que não aconteceu mente com um número à
# frente. Uma falha em qualquer amostra descarta a LINHA inteira, porque a
# mediana de um conjunto onde metade são erros não é a latência de nada.
time_n() {
  local -a samples=()
  local s e
  for _ in $(seq "$RUNS"); do
    s=$(date +%s%N)
    if ! "$@" >/dev/null 2>&1; then
      return 1
    fi
    e=$(date +%s%N)
    samples+=( $(( (e - s) / 1000000 )) )
  done
  stats "${samples[@]}"
}

# Uma linha medida vive aqui depois de impressa. O `row` antigo imprimia e
# esquecia, e é por isso que `--json` não tinha o que emitir: o número existia
# durante uma linha de terminal e mais nada.
declare -A RESULTS=()

# `$1` é a CHAVE (`docker`/`podman`/`delonix`) e `$2` a etiqueta legível. Uma
# ferramenta ausente não escreve entrada nenhuma — no JSON sai `null`, que é
# "não medido"; um `0` seria uma medição que ninguém fez.
row() {
  local key="$1"; shift
  local label="$1"; shift
  if ! command -v "$1" >/dev/null 2>&1 && [[ ! -x "$1" ]]; then
    printf '  %-28s %s\n' "$label" "não medido (ferramenta ausente)"
    return
  fi
  local r
  if ! r=$(time_n "$@"); then
    printf '  %-28s %s\n' "$label" "não medido (o comando falhou — ferramenta presente mas inutilizável)"
    return
  fi
  RESULTS[$key]="$r"
  printf '  %-28s %6s ms   (min %s, max %s, dispersão %sx)\n' "$label" \
    "$(cut -d'|' -f1 <<<"$r")" "$(cut -d'|' -f2 <<<"$r")" "$(cut -d'|' -f3 <<<"$r")" \
    "$(cut -d'|' -f5 <<<"$r")"
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
row docker  "docker (bridge)" docker run --rm alpine true
row podman  "podman (slirp)"  podman run --rm alpine true
row delonix "delonix (host)"  env DELONIX_ROOT="$SANDBOX/root" DELONIX_NET_RUNTIME_DIR="$SANDBOX/run" \
    "$BIN" container run --rm alpine true
echo
echo "Nota: os defaults NÃO são a mesma configuração. O do delonix é \`--net host\`;"
echo "para a comparação com rede isolada ver o \`docs/comparacao-medida.md\`, linha 4b."

# ---------------------------------------------------------------------------
# A saída máquina-legível (`--json`), no fd 3 — ver a nota de `--json` no topo.
#
# `publishable` é a mesma decisão que o relatório humano imprime em maiúsculas,
# não uma segunda opinião: sai do `BANCADA_OK` que o `bancada.sh` calculou. O
# `bench_gate.py` RECUSA-SE a julgar uma corrida com `false` aqui, que é o ponto
# — um número medido numa máquina carregada não acusa nem iliba ninguém.
jstr() { printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g'; }

json_line() {
  local key="$1" r="${RESULTS[$1]:-}"
  if [[ -z "$r" ]]; then
    printf '      "%s": null' "$key"
    return
  fi
  printf '      "%s": {"median_ms": %s, "min_ms": %s, "max_ms": %s, "spread": %s, "samples_ms": [%s]}' \
    "$key" "$(cut -d'|' -f1 <<<"$r")" "$(cut -d'|' -f2 <<<"$r")" "$(cut -d'|' -f3 <<<"$r")" \
    "$(cut -d'|' -f5 <<<"$r")" \
    "$(cut -d'|' -f4 <<<"$r" | tr -s ' ' | sed 's/ $//; s/ /, /g')"
}

json_tool() {
  if [[ -n "${TOOLVER[$1]:-}" ]]; then printf '"%s"' "$(jstr "${TOOLVER[$1]}")"; else printf 'null'; fi
}

if [[ "$JSON" == "1" ]]; then
  {
    printf '{\n'
    printf '  "schema": 1,\n'
    printf '  "when": "%s",\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    printf '  "runs": %s,\n' "$RUNS"
    printf '  "bench": {\n'
    printf '    "cpu": "%s",\n' "$(jstr "$CPU")"
    printf '    "threads": %s,\n' "$NCPU"
    printf '    "mem": "%s",\n' "$(jstr "$MEM")"
    printf '    "kernel": "%s",\n' "$(jstr "$KERNEL")"
    printf '    "load1": %s,\n' "$LOAD1"
    printf '    "threshold": %s,\n' "$THRESHOLD"
    printf '    "density": %s,\n' "$DENSITY"
    printf '    "publishable": %s\n' "$([[ "$BANCADA_OK" == 1 ]] && echo true || echo false)"
    printf '  },\n'
    printf '  "tools": {"docker": %s, "podman": %s, "delonix": %s},\n' \
      "$(json_tool docker)" "$(json_tool podman)" "$(json_tool delonix)"
    printf '  "lines": {\n'
    printf '    "4a": {\n'
    json_line docker;  printf ',\n'
    json_line podman;  printf ',\n'
    json_line delonix; printf '\n'
    printf '    }\n'
    printf '  }\n'
    printf '}\n'
  } >&3
fi
