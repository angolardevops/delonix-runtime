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

# Existe uma imagem local com este repo:tag? (lê `image ls -o json`, não grep de
# tabela — a saída estruturada é a verdade estável, ADR-0005.)
image_exists() { # REPO:TAG
  "${DELONIX}" image ls -o json 2>/dev/null | python3 -c '
import sys, json
tag = sys.argv[1]
try: data = json.load(sys.stdin)
except Exception: sys.exit(1)
for img in data or []:
    if tag in (img.get("repo_tags") or []):
        sys.exit(0)
sys.exit(1)
' "$1"
}

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

# --- rede de utilizador: DNS interno e alcançabilidade dirigida ---------------
# Resolução de nome PELO DNS interno, de DENTRO de um container (getent, musl).
# Ecoa o IP (primeira coluna da 1.ª linha) ou vazio se não resolver.
container_resolves() { # CONTAINER NOME -> IP ou vazio
  "${DELONIX}" container exec "$1" getent hosts "$2" 2>/dev/null | awk 'NR==1{print $1}'
}

# Sonda TCP feita DE DENTRO da rede — o único ponto de vista que vê a política.
# Ecoa exactamente UM token, para o chamador NÃO confundir três desfechos:
#   REACH  — a porta respondeu (ligação TCP estabelecida)
#   BLOCK  — recusada/sem rota (política ou serviço) dentro do timeout
#   NOTOOL — não há `nc` no provador: NÃO é bloqueio, é falta de ferramenta
_probe_body() { # ALVO PORTA TIMEOUT -> script sh que ecoa o token
  printf 'command -v nc >/dev/null 2>&1 || { echo NOTOOL; exit 0; }; if nc -z -w%s %s %s 2>/dev/null; then echo REACH; else echo BLOCK; fi' "$3" "$1" "$2"
}
# A partir de um container JÁ a correr (par com identidade — ex. a app autorizada).
container_reaches() { # CONTAINER ALVO PORTA [timeout_s]
  "${DELONIX}" container exec "$1" sh -c "$(_probe_body "$2" "$3" "${4:-3}")" 2>/dev/null | tail -1
}
# A partir de um container EFÉMERO novo na rede (o "terceiro" sem identidade).
ephemeral_reaches() { # REDE IMAGEM ALVO PORTA [timeout_s]
  "${DELONIX}" container run --rm --net "$1" "$2" sh -c "$(_probe_body "$3" "$4" "${5:-3}")" 2>/dev/null | tail -1
}

# Assere alcançabilidade do par AUTORIZADO com repetição (o alvo pode estar a
# aquecer — ex. o postgres demora a abrir o 5432). Distingue serviço-em-baixo de
# falta de ferramenta; só passa quando o token é exactamente REACH.
assert_reach() { # CONTAINER ALVO PORTA [timeout_s] [rótulo]
  local c="$1" host="$2" port="$3" timeout="${4:-20}" label="${5:-$1→$2:$3}" tok=""
  for _ in $(seq 1 "${timeout}"); do
    tok="$(container_reaches "$c" "$host" "$port" 2)"
    case "$tok" in
      REACH)  ok "${label}: porta ${port} responde (REACH, de dentro da rede)"; return 0 ;;
      NOTOOL) die "${label}: sem 'nc' em '${c}' — não consigo provar alcançabilidade (não é bloqueio, é ferramenta em falta)" ;;
    esac
    sleep 1
  done
  die "${label}: o par AUTORIZADO não alcança ${host}:${port} (último: ${tok:-vazio}) — serviço em baixo, NÃO política"
}

# --- cgroup v2 (limites rootless) --------------------------------------------
# PID do processo-raiz do container. `container inspect` devolve um ARRAY
# (ADR-0005), por isso lemos `[0].pid` — com python3, não com jq.
container_pid() { # NAME -> pid ou vazio
  "${DELONIX}" container inspect "$1" 2>/dev/null | python3 -c '
import sys, json
try: data = json.load(sys.stdin)
except Exception: sys.exit(0)
if isinstance(data, dict): data = [data]
for c in data or []:
    if c.get("pid"): print(c["pid"]); break
'
}

# Caminho relativo do cgroup v2 de um PID (a linha `0::` de /proc/PID/cgroup).
cgroup_path_of_pid() { # PID -> /user.slice/.../dlx-<id>  ou vazio
  awk -F: '/^0::/{print $3}' "/proc/$1/cgroup" 2>/dev/null
}

# Assere um ficheiro de cgroup REAL do kernel (a verdade, não a saída do CLI).
# Distingue TRÊS desfechos — não os confunde:
#   controlador ausente (ficheiro não existe) → die com mensagem própria
#   limite NÃO aplicado (valor == "max")       → die: delegação em falta / silêncio
#   valor != esperado                           → die: limite errado
# e só passa quando o valor é exactamente o esperado.
assert_cgroup() { # PID ficheiro esperado [rótulo]
  local pid="$1" file="$2" want="$3" label="${4:-$2}"
  local rel abs got
  rel="$(cgroup_path_of_pid "$pid")"
  [ -n "$rel" ] || die "${label}: não consegui ler o cgroup do PID ${pid}"
  abs="/sys/fs/cgroup${rel}/${file}"
  if [ ! -e "$abs" ]; then
    die "${label}: controlador AUSENTE (${file} não existe em ${rel}). Isto NÃO é 'limite não aplicado' — é o controlador que não desceu até ti (ex.: cpuset/io num host de fábrica)."
  fi
  got="$(cat "$abs" 2>/dev/null || true)"
  if [ "$got" = "max" ]; then
    die "${label}: limite NÃO aplicado (${file}=max). A delegação de cgroup não está feita — o container corre SEM tecto, em silêncio. É exactamente o que o lab te manda provar."
  fi
  if [ "$got" != "$want" ]; then
    die "${label}: esperava ${file}=${want}, li ${got} do cgroup REAL — o limite não pegou."
  fi
  ok "${label}: ${file}=${got} (lido do cgroup REAL do kernel, não da saída do comando)"
}
