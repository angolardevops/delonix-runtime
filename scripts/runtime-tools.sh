#!/usr/bin/env bash
# Conformância OCI RUNTIME: corre a *validation suite* do `opencontainers/runtime-tools`
# contra um runtime OCI (o CLI estilo runc: `create/start/state/kill/delete <id> --bundle`).
# É o par do `critest.sh` (CRI), para o Track B do plano de conformidade e o gate da
# Fase 1 do ADR-0080 (`delonix oci-runtime`).
#
# Uso:   ./scripts/runtime-tools.sh [-- <args extra do prove>]
# Env:   RUNTIME=<path>   runtime a medir. Default `runc` (a REFERÊNCIA). Para medir o
#                         Delonix quando o RT-4 existir: um wrapper que traduza o CLI runc
#                         para `delonix oci-runtime …` (ver RT4_WRAPPER abaixo).
#        RT_VERSION=<ref> tag/commit do runtime-tools a usar. Default é o commit já no
#                         go mod cache (compila sob Go 1.25; o v0.9.0 NÃO compila).
#        FOCUS=<regex>    corre só os casos cujo nome bate (grep sobre a lista de .t).
#        SKIP=<regex>     exclui os casos que batem.
#        KEEP=1           não apaga o checkout/compilação no fim (para depurar).
#
# Pré-requisitos: Go no PATH (compila a suite) e o RUNTIME. NÃO precisa de root para
# correr — mas a MAIORIA dos casos da suite exige privilégio (namespaces, cgroups,
# mounts); em rootless passam só os que não o exigem, e isso é uma propriedade do
# AMBIENTE, não do runtime (o `critest.sh` tem a mesma nota). Para o número comparável
# ao runc, corre-se os dois no MESMO modo (ambos root, ou ambos rootless).
#
# Saída: TAP agregado pelo `prove`, e uma linha-resumo `== runtime-tools: N ok / M not ok ==`
# que é o baseline a publicar (como o `Ran X of Y … Passed|Failed` do critest).
set -uo pipefail

RUNTIME="${RUNTIME:-runc}"
RT_VERSION="${RT_VERSION:-v0.9.1-0.20261017090856-bd3b0bd00f44}"
export PATH="$PATH:/usr/local/go/bin"

command -v go >/dev/null || { echo "ERRO: go não está no PATH (precisa de compilar a suite)"; exit 2; }
command -v "$RUNTIME" >/dev/null || { echo "ERRO: RUNTIME '$RUNTIME' não está no PATH"; exit 2; }
command -v prove >/dev/null || { echo "ERRO: 'prove' (Perl TAP::Harness) não está no PATH — instala perl"; exit 2; }

# Isolamento: um checkout e um TMPDIR próprios por execução (mktemp + PID), para não
# colidir com outra sessão nem com o critest a correr em paralelo.
WORK="$(mktemp -d "/tmp/delonix-runtime-tools-$$.XXXXXX")"
cleanup() { [[ "${KEEP:-0}" == 1 ]] || rm -rf "$WORK"; }
trap cleanup EXIT

echo "== bancada =="
echo "  runtime:        $RUNTIME ($("$RUNTIME" --version 2>&1 | head -1))"
echo "  runtime-tools:  $RT_VERSION"
echo "  go:             $(go version)"
echo "  modo:           $([[ $(id -u) -eq 0 ]] && echo root || echo 'rootless (subconjunto — ver nota no topo)')"

# 1) Fonte da suite. Reusa o go mod cache (já lá está) copiando-o para um dir gravável,
#    porque o cache é read-only e o `make` escreve binários na árvore.
SRC="$WORK/runtime-tools"
CACHED="$(go env GOMODCACHE)/github.com/opencontainers/runtime-tools@${RT_VERSION}"
if [[ -d "$CACHED" ]]; then
  cp -a "$CACHED" "$SRC"; chmod -R u+w "$SRC"
else
  echo "== a obter runtime-tools@$RT_VERSION (não estava no cache) =="
  git clone --quiet https://github.com/opencontainers/runtime-tools "$SRC" || { echo "ERRO: clone falhou"; exit 2; }
  ( cd "$SRC" && git checkout --quiet "$RT_VERSION" 2>/dev/null || true )
fi

# 2) Compila `runtimetest` (o agente que corre DENTRO do container e verifica o estado)
#    e os executáveis de validação (um .t por área do spec).
echo "== a compilar a validation suite (go) =="
( cd "$SRC" && make runtimetest validation-executables ) >"$WORK/build.log" 2>&1 || {
  echo "ERRO: a compilação da suite falhou — ver $WORK/build.log"; tail -20 "$WORK/build.log"; exit 3;
}

# 3) Descobre os casos (.t), aplica FOCUS/SKIP.
mapfile -t CASES < <(find "$SRC/validation" -maxdepth 2 -name '*.t' -type f | sort)
[[ -n "${FOCUS:-}" ]] && mapfile -t CASES < <(printf '%s\n' "${CASES[@]}" | grep -E "$FOCUS")
[[ -n "${SKIP:-}"  ]] && mapfile -t CASES < <(printf '%s\n' "${CASES[@]}" | grep -Ev "$SKIP")
[[ ${#CASES[@]} -gt 0 ]] || { echo "ERRO: nenhum caso de validação após FOCUS/SKIP"; exit 2; }
echo "== $(printf '%s\n' "${CASES[@]}" | wc -l) casos a correr contra RUNTIME=$RUNTIME =="

# 4) Corre pelo `prove` (TAP). `RUNTIME` e `RUNTIMETEST` são o contrato da suite.
export RUNTIME RUNTIMETEST="$SRC/runtimetest"
LOG="$WORK/prove.log"
prove -v "${CASES[@]}" "$@" >"$LOG" 2>&1
rc=$?

# 5) Resumo (o baseline a publicar).
ok=$(grep -cE '^ok '     "$LOG" 2>/dev/null || echo 0)
notok=$(grep -cE '^not ok' "$LOG" 2>/dev/null || echo 0)
echo "== runtime-tools: ${ok} ok / ${notok} not ok (rc=$rc) =="
echo "   log completo: ${KEEP:+$WORK/prove.log}${KEEP:-(use KEEP=1 para o preservar)}"
[[ "${KEEP:-0}" == 1 ]] && echo "   (checkout/binaries preservados em $WORK)"
exit $rc
