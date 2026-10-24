# shellcheck shell=bash
# Biblioteca partilhada dos labs executáveis do Delonix.
#
# Modelo de lab (docs §7): cada lab é quatro fases idempotentes —
#   prepare.sh  → garante pré-requisitos e deixa o terreno pronto
#   run.sh      → executa a jornada (os comandos que o leitor aprende)
#   verify.sh   → ASSERÇÃO de resultado FUNCIONAL (não um exit-code isolado)
#   cleanup.sh  → remove SÓ o que o lab criou
# e um orquestrador lab.sh que corre as quatro em ordem.
#
# Cada fase faz `source` desta biblioteca. O binário é resolvido por
# `${DELONIX:-delonix}`: um leitor que instalou pelo install.sh não define nada;
# a validação ao vivo aponta DELONIX para o binário construído.
#
# O estado entre fases (nomes, portas) vive num ficheiro no diretório de estado,
# para que `verify` e `cleanup` saibam exatamente o que `run` criou.

set -euo pipefail

DELONIX="${DELONIX:-delonix}"

# Diretório de estado do lab (sobrescrevível para correr vários em paralelo).
LAB_STATE_DIR="${LAB_STATE_DIR:-${TMPDIR:-/tmp}/delonix-lab-${LAB_ID:-unknown}}"
LAB_STATE_FILE="${LAB_STATE_DIR}/state.env"

mkdir -p "${LAB_STATE_DIR}"

# --- saída legível -----------------------------------------------------------
_c() { printf '\033[%sm' "$1" 2>/dev/null || true; }
log()   { printf '%s▸%s %s\n' "$(_c 36)" "$(_c 0)" "$*"; }
ok()    { printf '%s✓%s %s\n' "$(_c 32)" "$(_c 0)" "$*"; }
warn()  { printf '%s!%s %s\n' "$(_c 33)" "$(_c 0)" "$*" >&2; }
die()   { printf '%s✗%s %s\n' "$(_c 31)" "$(_c 0)" "$*" >&2; exit 1; }

# --- estado partilhado entre fases -------------------------------------------
state_set() { # chave valor
  local k="$1" v="$2"
  touch "${LAB_STATE_FILE}"
  grep -v "^${k}=" "${LAB_STATE_FILE}" > "${LAB_STATE_FILE}.tmp" 2>/dev/null || true
  mv "${LAB_STATE_FILE}.tmp" "${LAB_STATE_FILE}"
  printf '%s=%q\n' "$k" "$v" >> "${LAB_STATE_FILE}"
}
# shellcheck disable=SC1090  # caminho de estado em runtime, não constante
state_load() { [ -f "${LAB_STATE_FILE}" ] && . "${LAB_STATE_FILE}" || true; }

# --- pré-requisitos ----------------------------------------------------------
need() { command -v "$1" >/dev/null 2>&1 || die "em falta no PATH: $1"; }

# --- consulta de containers (saída JSON estável, ADR-0005) -------------------
# `delonix container ps` não imita o `--format` do Docker: a saída estruturada
# é `-o json`. Lemos os campos com python3 (sempre presente) e não com jq.
_ps_json() { "${DELONIX}" container ps -a -o json 2>/dev/null; }
container_field() { # NAME CAMPO(name|status|image|ports|id) -> valor ou vazio
  _ps_json | python3 -c '
import sys, json
name, field = sys.argv[1], sys.argv[2]
try: data = json.load(sys.stdin)
except Exception: sys.exit(0)
for c in data or []:
    if c.get("name") == name:
        print(c.get(field, "")); break
' "$1" "$2"
}
container_exists() { [ -n "$(container_field "$1" name)" ]; }

delonix_ok() {
  if ! "${DELONIX}" version >/dev/null 2>&1 && ! "${DELONIX}" --version >/dev/null 2>&1; then
    die "o binário '${DELONIX}' não responde. Instala o Delonix ou define DELONIX=<caminho>."
  fi
  ok "delonix presente: $("${DELONIX}" --version 2>/dev/null | head -1)"
}

# Primeira porta TCP livre a partir de $1 (default 18080) — evita colisões.
free_port() {
  local p="${1:-18080}"
  while ss -ltn 2>/dev/null | awk '{print $4}' | grep -qE "[:.]${p}$"; do
    p=$((p + 1))
  done
  printf '%s' "$p"
}

# --- asserções funcionais ----------------------------------------------------
# Repete até ao timeout: o resultado FUNCIONAL, não um exit-code instantâneo.
assert_http() { # url codigo_esperado [timeout_s]
  local url="$1" want="${2:-200}" timeout="${3:-20}" code=""
  for _ in $(seq 1 "${timeout}"); do
    code="$(curl -s -o /dev/null -w '%{http_code}' "$url" 2>/dev/null || echo 000)"
    [ "$code" = "$want" ] && { ok "HTTP ${code} em ${url}"; return 0; }
    sleep 1
  done
  die "esperava HTTP ${want} em ${url}, obtive ${code}"
}

assert_contains() { # ficheiro_ou_stdin agulha mensagem
  local hay="$1" needle="$2" msg="${3:-conteúdo esperado}"
  if printf '%s' "$hay" | grep -qF -- "$needle"; then ok "$msg"; else die "$msg (não encontrei: $needle)"; fi
}
