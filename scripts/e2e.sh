#!/usr/bin/env bash
# E2E de toda a superfície da CLI `delonix` — corre cada comando/subcomando real
# e regista PASS/FAIL/SKIP num relatório. NÃO é um teste unitário: toca no
# estado real da máquina (containers, redes, volumes), por isso limpa atrás de si.
#
# Uso:  ./scripts/e2e.sh [caminho-do-binario]
# Saída: relatório em stdout + JSONL detalhado em $OUT/results.jsonl
#
# Regra: NUNCA usar o `delonix` do PATH — processos/binários antigos são uma
# armadilha conhecida deste repo (ver AGENTS.md). O default é o build local.
#
# ## Código de saída — o que este portão chumba, e o que não
#
#   FAIL  > 0  -> 1.  Um check chumbou e não há achado escrito por trás.
#   XPASS > 0  -> 1.  Um check marcado como defeito conhecido PASSOU: o defeito
#                     foi corrigido e a marca tem de sair (ver `xfail`).
#   SKIP  > 0  -> 0, mas em bloco próprio no resumo. Uma medição que não se pôde
#                     fazer não é um resultado negativo — e também não é um
#                     verde, que é como se lia quando saía diluída nas
#                     seiscentas linhas acima.
#   XFAIL > 0  -> 0.  Chumba por defeito com achado escrito. Fica na bateria,
#                     fora do portão, e chumba o portão no dia em que passar.
#
# Até 2026-09-09 esta linha era `exit 0` incondicional, com a justificação de
# que «o relatório é o produto». Medido nesse dia: PASS=529 FAIL=36 SKIP=5, e
# `echo $?` a dizer 0 — qualquer passo de CI construído por cima era decorativo.
#
# ## O que o número quer dizer, e o que NÃO quer (medido 2026-09-09, v3.0.0)
#
# A CLI tem 276 comandos e **244 folhas invocáveis** (`scripts/cli-tree.sh
# --leaves`). Esta bateria verifica o `--help` de **244/244 — 100%**, e agora
# por CONSTRUÇÃO: o ciclo lê o inventário do `cli-tree.sh`, em vez da lista de
# grupos escrita à mão que lá estava (que dizia 100% e media 167 de 244 — 68%).
#
# **EXECUTA 91 — 37%.** As outras 153 têm o contrato verificado e nunca são
# corridas, concentradas em `net` (28), `image` (25), `vm` (19), `container`
# (16), `system` (14) e `cluster` (11).
#
# Cita-se a FRACÇÃO medida e a data, nunca o total de checks: um total que sobe
# faz a cobertura parecer melhor sem uma única folha nova exercitada — e é
# literalmente o que aconteceu aqui, com o total a passar de 570 para 688 sem
# nenhuma folha nova a ser EXECUTADA.
#
# Um verde aqui lê-se com facilidade como «a CLI foi testada», e o que foi
# testado é sobretudo o texto de ajuda: foi em comandos nunca executados que a
# auditoria encontrou um errno cru (`node init`) e um `create` de overlay a sair
# 0 sobre uma rede por realizar.
#
# **E há uma terceira forma de não testar nada, medida 2026-09-09:** um check que
# chumba sempre. Trinta e seis chumbavam, e trinta e quatro deles por grafias
# removidas na v2.0.0/v3.0.0 (`volumes`, `schema print`, `pod ls`, `net boot`,
# `vm status`, `net httproute ls`, `container inspect -o json`) — não por
# defeitos do motor. Entre eles estavam os três que verificam que um `-m`
# declarado CHEGA ao cgroup: chumbavam com `_cg_of: command not found`, porque a
# função não atravessava o `bash -c` do `check`. A verificação que existe por
# causa do bug do `64Mi` nunca tinha corrido uma única vez, escondida atrás de um
# vermelho que se lia como defeito conhecido.
#
# ## E a QUARTA forma: um check que chumba de vez em quando
#
# Assim que o portão passou a valer, três checks começaram a piscar. Nenhum era
# regressão — a mesma árvore e o mesmo binário deram FAIL=0 na corrida seguinte,
# a load MAIS ALTA. O que estava errado eram as PRÉ-CONDIÇÕES, e nos três casos
# da mesma maneira: o check media uma coisa e dependia, sem o dizer, de outra.
#
#   `CH: rm com a VM parada` — o `stop` do cloud-hypervisor manda SIGTERM e
#   devolve SEM esperar que o VMM saia, e o `qemu-img` a seguir apanhava o lock
#   do qcow2 ainda tomado. Passou a esperar pelo LOCK (a condição de que
#   depende), não pelo `stop`. Suspeita de defeito registada como ACH-014, sem
#   marca: uma ocorrência e zero reproduções em 20 tentativas não é um defeito
#   medido, e uma marca afirmaria mais do que se sabe.
#
#   `os dados voltaram` — a causa não era a janela (20s não bastaram). Era que a
#   pré-condição em cima — escrever no volume — era um ciclo MUDO: fazia
#   `break` no sucesso e caía em silêncio ao fim de 50 tentativas. Um volume
#   vazio arquiva-se e restaura-se sem erro, e o vermelho aparecia três checks
#   depois do sítio onde o problema estava — a mesma misdiagnose que esta secção
#   já dizia ter corrigido. Agora é um `check` com nome, e o arquivo passou a ser
#   verificado por DENTRO: a entrada `volumes/x.tar.gz` não são os dados.
#
# ## Isolamento: fá-lo por si desde 2026-08-15 (como o `chaos.sh`)
#
# **Isola-se por omissão desde 2026-08-15.** Redirecciona `DELONIX_ROOT` E
# `DELONIX_NET_RUNTIME_DIR` para directórios próprios, cria-os, e derruba a infra
# que subiu ao sair. Limpa atrás de si e prefixa tudo
# o que cria (`$PFX`), mas uma corrida interrompida a meio deixa restos, e num
# host com produção a correr isso é risco directo. Para isolar, exporta os dois
# roots ANTES de invocar:
#
#   DELONIX_ROOT=/tmp/e2e/root DELONIX_NET_RUNTIME_DIR=/tmp/e2e/run ./scripts/e2e.sh
#
# Os DOIS, sempre — meia-isolação é pior que nenhuma. Os sockets de rede são por
# UTILIZADOR e os pidfiles por ROOT: isolar só o `DELONIX_ROOT` põe dois roots a
# disputar `/tmp/delonix-net-<uid>/`, e o fim disso (medido, 2026-08-12) é o root
# isolado subir um pin/slirp por cima dos mesmos caminhos e a reconciliação
# seguinte, corrida do root real, reconstruir a infra e reiniciar containers de
# produção.
#
# **Porque deixou de não ser o default.** A objecção registada era que «os checks
# que dependem de estado real (imagens no store, holder a correr) passariam a
# falhar em vez de exercitar». Medido: não passam — a secção `image` já faz
# `pull` quando a referência não está no store, que é exactamente o caso de um
# root vazio. A única dependência real que sobra é REDE, e essa é declarada: sem
# ela o pull falha e as secções que precisam de imagem **saltam com a razão**, em
# vez de pintarem a bateria de vermelho.
#
# E o custo de NÃO isolar era concreto: duas secções acrescentadas nesta série
# (`net`, `compose`) já se recusavam a correr sem os dois roots, ou seja o
# isolamento estava a tornar-se o default por acumulação, sem ninguém o ter
# decidido — meio-feito, que é a pior das três hipóteses e é literalmente o
# incidente de 2026-08-12 registado no AGENTS.md.
#
# `E2E_SHARED_STATE=1` opta por sair disto e correr contra a máquina real (o
# comportamento anterior), com aviso alto. Serve para diagnosticar um host, não
# para uma corrida normal.

set -uo pipefail

BIN="${1:-$(cd "$(dirname "$0")/.." && pwd)/target/debug/delonix}"
OUT="${OUT:-/tmp/delonix-e2e}"
mkdir -p "$OUT"
: >"$OUT/results.jsonl"

# --- isolamento (ver cabeçalho) ---------------------------------------------
if [[ "${E2E_SHARED_STATE:-0}" == "1" ]]; then
  E2E_ISOLATED=0
else
  E2E_ISOLATED=1
  # O runtime dir tem de ser CURTO: `sun_path` do AF_UNIX são 108 bytes, e um
  # `$OUT` fundo (o default de uma sessão de agente já passa os 90) põe o socket
  # de controlo acima do limite. Não deriva do `$OUT` por isso.
  export DELONIX_ROOT="${DELONIX_ROOT:-$OUT/root}"
  export DELONIX_NET_RUNTIME_DIR="${DELONIX_NET_RUNTIME_DIR:-/tmp/dlxe2e-$$}"
  mkdir -p "$DELONIX_ROOT" "$DELONIX_NET_RUNTIME_DIR"
  if [[ ${#DELONIX_NET_RUNTIME_DIR} -gt 80 ]]; then
    echo "FATAL: DELONIX_NET_RUNTIME_DIR tem ${#DELONIX_NET_RUNTIME_DIR} bytes;" >&2
    echo "       o socket de controlo passaria o limite de 108 do AF_UNIX." >&2
    exit 2
  fi
  # A infra que ESTA corrida subir é desta corrida — desce ao sair, mesmo por
  # Ctrl-C. Sem isto um pin/slirp isolado fica a correr depois do relatório.
  trap '"$BIN" net netns down >/dev/null 2>&1 || true' EXIT
fi

PASS=0; FAIL=0; SKIP=0; XFAIL=0; XPASS=0
declare -a FAILED_NAMES=()
declare -a SKIPPED_NAMES=()
declare -a XFAIL_NAMES=()
declare -a XPASS_NAMES=()

# Prefixo único para tudo o que este teste cria — para a limpeza nunca tocar em
# recursos do utilizador.
PFX="e2e$$"

log() { printf '%s\n' "$*"; }

# check <nome> <expectativa: ok|fail|<n>> <comando...>
#   ok   = esperamos RC=0
#   fail = esperamos RC!=0 (testes de erro: a CLI tem de RECUSAR, não aceitar)
#   <n>  = esperamos EXACTAMENTE esse RC. `fail` só prova que a CLI recusou; a
#          CLASSE da recusa (3 parado / 4 inexistente / 5 conflito — ver
#          docs/cli-stability.md) é a parte que um reconciliador lê, e um
#          `fail` continuaria verde se todas voltassem a colapsar em 1.
check() {
  local name="$1" expect="$2"; shift 2
  local out rc
  out="$("$@" 2>&1)"; rc=$?
  local verdict=FAIL
  if [[ "$expect" == "ok" ]]; then
    [[ $rc -eq 0 ]] && verdict=PASS
  elif [[ "$expect" == "fail" ]]; then
    [[ $rc -ne 0 ]] && verdict=PASS
  else
    [[ $rc -eq "$expect" ]] && verdict=PASS
  fi
  python3 - "$name" "$verdict" "$rc" "$out" "$*" >>"$OUT/results.jsonl" <<'PY'
import json,sys
name,verdict,rc,out,cmd=sys.argv[1:6]
print(json.dumps({"name":name,"verdict":verdict,"rc":int(rc),"cmd":cmd,"output":out[:4000]}))
PY
  if [[ $verdict == PASS ]]; then
    PASS=$((PASS+1)); log "  PASS  $name"
  else
    FAIL=$((FAIL+1)); FAILED_NAMES+=("$name")
    log "  FAIL  $name  (rc=$rc, esperado=$expect)"
    log "        $ $*"
    sed 's/^/        | /' <<<"$out" | head -8
  fi
}

# `${2:-}` de propósito: com `set -u`, um `skip` com um argumento só (três
# deles existiam nesta bateria, todos no ramo de erro da secção de limites)
# matava o script inteiro a meio, com «unbound variable» e sem relatório. Um
# harness que morre por causa de uma gralha no seu próprio ramo de excepção é a
# pior forma de não medir nada.
skip() {
  local name="$1" reason="${2:-sem razão declarada}"
  SKIP=$((SKIP+1)); SKIPPED_NAMES+=("$name — $reason"); log "  SKIP  $name  — $reason"
  python3 -c 'import json,sys; print(json.dumps({"name":sys.argv[1],"verdict":"SKIP","reason":sys.argv[2]}))' \
    "$name" "$reason" >>"$OUT/results.jsonl"
}

# --- defeitos JÁ CONHECIDOS: o ratchet, não uma lista de desculpas -----------
#
# Um check que chumba por um defeito com achado escrito não pode chumbar o
# portão todos os dias — ao fim de uma semana ninguém olha para o vermelho. Mas
# apagá-lo é pior: o defeito desaparece da bateria e volta a ser descoberto do
# zero. `xfail` é o meio-termo com dentes:
#
#   FAIL esperado  -> XFAIL, não chumba, sai em bloco próprio com o achado.
#   PASS inesperado-> XPASS, CHUMBA. O defeito foi corrigido: tira a marca.
#
# O XPASS chumbar é o ponto todo. Sem isso a marca fica para sempre e a bateria
# passa a testar menos do que diz. Funcionou à primeira, e duas vezes no mesmo
# dia: os quatro checks da share entraram como `xfail ACH-001` e saíram quando o
# #256 corrigiu o defeito; o do membro por omissão entrou como `xflaky ACH-011` e
# saiu quando o #258 o corrigiu.
#
# Neste momento os DOIS não têm utilizadores, e isso é o estado BOM — não código
# morto. A marca é para o intervalo entre descobrir e corrigir; um ficheiro sem
# marcas nenhumas quer dizer que não há defeito conhecido a fingir de verde. Não
# os apagues por estarem sem uso: apagá-los é tirar o único sítio onde o próximo
# defeito conhecido pode ficar visível sem chumbar o portão todos os dias.
# O JSONL guarda o veredicto CRU do `check` (FAIL/PASS) e, logo a seguir, a
# decisão que se tomou sobre ele. Reescrever a linha anterior seria mais bonito
# e mentiria sobre o que o comando fez.
_xrec() {
  python3 -c 'import json,sys; print(json.dumps({"name":sys.argv[1],"verdict":sys.argv[2],"achado":sys.argv[3]}))' \
    "$1" "$2" "$3" >>"$OUT/results.jsonl"
}

xfail() {
  local achado="$1" name="$2"; shift 2
  local before_fail=$FAIL before_pass=$PASS
  check "$name" "$@"
  if (( FAIL > before_fail )); then
    FAIL=$((FAIL-1)); XFAIL=$((XFAIL+1))
    FAILED_NAMES=("${FAILED_NAMES[@]:0:${#FAILED_NAMES[@]}-1}")
    XFAIL_NAMES+=("$achado: $name")
    _xrec "$name" XFAIL "$achado"
    log "        ^ XFAIL — chumba por $achado (defeito conhecido), não chumba o portão"
  elif (( PASS > before_pass )); then
    PASS=$((PASS-1)); XPASS=$((XPASS+1))
    XPASS_NAMES+=("$achado: $name")
    _xrec "$name" XPASS "$achado"
    log "  XPASS $name — $achado já não reproduz: tira o \`xfail\` desta linha"
  fi
}

# A variante para um defeito cuja natureza É a intermitência. Um `xfail` normal
# aqui daria XPASS metade das corridas e chumbava o portão por o defeito não ter
# batido nesta — que é o mesmo ruído que este trabalho existe para tirar. Nunca
# chumba, nos dois sentidos, e diz sempre porquê.
xflaky() {
  local achado="$1" name="$2"; shift 2
  local before_fail=$FAIL before_pass=$PASS
  check "$name" "$@"
  if (( FAIL > before_fail )); then
    FAIL=$((FAIL-1)); XFAIL=$((XFAIL+1))
    FAILED_NAMES=("${FAILED_NAMES[@]:0:${#FAILED_NAMES[@]}-1}")
    XFAIL_NAMES+=("$achado (intermitente): $name")
    _xrec "$name" XFAIL "$achado (intermitente)"
  elif (( PASS > before_pass )); then
    PASS=$((PASS-1)); XFAIL=$((XFAIL+1))
    XFAIL_NAMES+=("$achado (intermitente, passou NESTA corrida): $name")
    _xrec "$name" XFAIL "$achado (intermitente, passou nesta corrida)"
  fi
}

section() { log ""; log "=== $1 ==="; }

[[ -x "$BIN" ]] || { log "binário não executável: $BIN"; exit 1; }
log "binário: $BIN"
log "versão:  $("$BIN" --version 2>&1)"

########################################
section "help / superfície da CLI"
########################################
check "help raiz" ok "$BIN" --help
check "version" ok "$BIN" --version
# O `--help` de TODAS as folhas invocáveis, e o inventário vem do
# `cli-tree.sh --leaves` — não de uma lista de grupos escrita à mão.
#
# A lista à mão era `container image build vm volumes network stack system
# cluster completion`, e custou duas coisas de uma vez (medido 2026-09-09):
#
#   1. `volumes` deixou de existir no B2. O `help de 'volumes'` chumbava — isso
#      via-se. O que NÃO se via é que o ciclo dos subcomandos derivava a lista
#      de `"$BIN" volumes --help`, que passou a devolver VAZIO: as dez folhas
#      do grupo `volume` perderam o check de `--help` sem uma única linha
#      vermelha a dizê-lo. Uma cobertura que encolhe em silêncio.
#   2. Faltavam grupos inteiros — `net`, `pod`, `compose`, `backup`, `secret`,
#      `serve`, `manifest`, `mcp`, `config`, `workload`. E os subgrupos nunca
#      entravam: `net netns up` está dois níveis abaixo e o ciclo só descia um.
#
# Resultado medido antes desta mudança: 167 das 244 folhas com `--help`
# verificado (68%), contra os «100%» que o cabeçalho deste ficheiro anunciava.
# Percorrer a árvore põe os 244 lá por CONSTRUÇÃO, e uma folha nova entra na
# bateria no dia em que nasce.
LEAVES="$OUT/leaves.txt"
if DELONIX_BIN="$BIN" bash "$(dirname "$0")/cli-tree.sh" --leaves >"$LEAVES" 2>/dev/null \
   && [[ -s "$LEAVES" ]]; then
  _nleaves=$(grep -c . "$LEAVES")
  log "  (o --help de $_nleaves folhas, inventário do cli-tree.sh)"
  while read -r _leaf; do
    [[ -n "$_leaf" ]] || continue
    # shellcheck disable=SC2086
    check "help de '$_leaf'" ok "$BIN" $_leaf --help
  done <"$LEAVES"
else
  skip "help de todas as folhas" "o cli-tree.sh não produziu inventário"
fi

########################################
section "comandos de leitura (não destrutivos)"
########################################
check "container ls" ok "$BIN" container ls
check "container ls -a" ok "$BIN" container ls -a
check "container ls -q" ok "$BIN" container ls -q
check "image ls" ok "$BIN" image ls
check "image vm ls" ok "$BIN" image vm ls
check "volume ls" ok "$BIN" volume ls
check "network ls" ok "$BIN" network ls

# --- `-n/--namespace` FILTRA mesmo, e a coluna esconde-se (A-2/A-3) --------
# A armadilha que isto existe para apanhar é a que este repo já corrigiu três
# vezes (`--security-opt seccomp=`, `-v …:z`, `--network-alias`): uma flag
# ACEITE e depois IGNORADA. Um check do `--help` passaria com a filtragem por
# ligar — por isso estes EXECUTAM e comparam contagens.
ns_rows() { "$BIN" $1 2>/dev/null | tail -n +2 | grep -c . || true; }
for grupo in "container ps -a" "workload ls" "get pods"; do
  todos=$(ns_rows "$grupo")
  # Um namespace que não existe tem de dar ZERO. Se a flag fosse ignorada daria
  # `$todos` — que é exactamente o sintoma do aceite-e-ignorado.
  check "$grupo -n inexistente devolve zero" ok bash -c \
    "[ \"\$('$BIN' $grupo -n zzz-nao-existe-$PFX 2>/dev/null | tail -n +2 | grep -c . || true)\" -eq 0 ]"
  # E `-n default` não pode devolver MENOS do que existe num host sem namespaces.
  check "$grupo -n default não esconde nada" ok bash -c \
    "[ \"\$('$BIN' $grupo -n default 2>/dev/null | tail -n +2 | grep -c . || true)\" -le $todos ]"
done
# A coluna esconde-se sem namespaces e aparece quando o filtro a nomeia — as
# duas metades da mesma regra (`output::namespace_cell` + `drop_uninformative`).
#
# Mas só com LINHAS. `drop_uninformative` não deita nada fora de uma tabela
# vazia, e isso é decidido, documentado e tem teste em Rust: «sem linhas não há
# prova de que uma coluna seja inútil, e o cabeçalho é a única coisa que um `ls`
# sem resultados tem para dizer».
#
# A versão anterior deste check exigia a coluna escondida sempre, e por isso
# chumbava numa raiz virgem — que é EXACTAMENTE o estado normal desta bateria
# desde que passou a isolar-se. Passava por acidente quando havia containers de
# uma corrida anterior. Um check cujo veredicto depende do que sobrou da última
# vez não mede o motor, mede a máquina; e este chumbava a acusar de defeito uma
# decisão de desenho.
_ps_rows=$("$BIN" container ps -a 2>/dev/null | tail -n +2 | grep -c . || true)
if (( _ps_rows > 0 )); then
  check "com linhas todas em default, a coluna NAMESPACE esconde-se" ok bash -c \
    "! '$BIN' container ps -a | head -1 | grep -q NAMESPACE"
else
  check "numa tabela VAZIA nada se esconde (o cabeçalho é tudo o que há)" ok bash -c \
    "'$BIN' container ps -a | head -1 | grep -q NAMESPACE"
fi
check "com -n default a coluna aparece" ok bash -c \
  "'$BIN' container ps -a -n default | head -1 | grep -q NAMESPACE"
check "vm ls" ok "$BIN" vm ls
check "get clusters" ok "$BIN" get clusters

# `cluster kubeconfig` só lê `<root>/clusters/<nome>-kubeconfig.yaml` — não
# distingue modo kind de kubeadm/SSH, e não precisa de nenhum dos dois vivo
# para se provar. Booting um cluster real aqui (cgroup delegado + download do
# `kindest/node`) provaria o `cluster create`, não este comando; um ficheiro
# fabricado no formato real prova exactamente o que o comando faz: ler,
# resolver 0/1/muitos, e nunca adivinhar.
check "cluster kubeconfig sem nenhum em cache recusa" fail "$BIN" cluster kubeconfig
mkdir -p "$DELONIX_ROOT/clusters"
cat >"$DELONIX_ROOT/clusters/ck1-$PFX-kubeconfig.yaml" <<YAML
apiVersion: v1
kind: Config
clusters:
  - name: ck1-$PFX
    cluster: {server: https://127.0.0.1:6443}
users:
  - name: ck1-$PFX-admin
    user: {token: fake-e2e-token}
contexts:
  - name: ck1-$PFX
    context: {cluster: ck1-$PFX, user: ck1-$PFX-admin}
current-context: ck1-$PFX
YAML
check "cluster kubeconfig sem nome resolve ao único em cache" ok bash -c \
  "'$BIN' cluster kubeconfig | grep -q 'ck1-$PFX'"
check "cluster kubeconfig <nome> imprime esse" ok bash -c \
  "'$BIN' cluster kubeconfig 'ck1-$PFX' | grep -q 'fake-e2e-token'"
check "cluster kubeconfig <inexistente> recusa" fail "$BIN" cluster kubeconfig "nao-existe-$PFX"
cp "$DELONIX_ROOT/clusters/ck1-$PFX-kubeconfig.yaml" "$DELONIX_ROOT/clusters/ck2-$PFX-kubeconfig.yaml"
check "cluster kubeconfig sem nome com vários recusa e nomeia-os" ok bash -c \
  "out=\$('$BIN' cluster kubeconfig 2>&1); rc=\$?; [ \$rc -ne 0 ] && echo \"\$out\" | grep -q \"ck1-$PFX\" && echo \"\$out\" | grep -q \"ck2-$PFX\""
rm -f "$DELONIX_ROOT/clusters/ck1-$PFX-kubeconfig.yaml" "$DELONIX_ROOT/clusters/ck2-$PFX-kubeconfig.yaml"

check "system info" ok "$BIN" system info
check "system df" ok "$BIN" system df
# O `df` conta a raiz INTEIRA. Medido a 2026-09-09 num nó real: um estado de
# 190 GiB reportava 105 — `vms/`, `build-cache/` e `images-build/` não estavam
# na lista, e nada o dizia. Um `ok` não apanha isto (o comando sempre devolveu
# 0 a omitir metade do disco); o que fixa é a PRESENÇA das linhas e do total.
check "system df conta os discos de VM" ok bash -c "'$BIN' system df | grep -q '^VM disks '"
check "system df conta a cache de build" ok bash -c "'$BIN' system df | grep -q '^build cache '"
check "system df fecha a tabela com other e TOTAL" ok bash -c "'$BIN' system df | grep -q '^other ' && '$BIN' system df | grep -q '^TOTAL '"
check "system events" ok "$BIN" system events
check "completion shell bash" ok "$BIN" completion shell bash

# --- os NOMES completam-se, e não só o script de registo (C-2) ------------
# O `completion shell bash` acima prova que o script de registo SAI; não prova que um
# TAB sobre um argumento sugere alguma coisa. A distinção não é teórica: o
# `image vm rm` — o comando DESTRUTIVO — não sugeria nada enquanto o `describe`
# ao lado sugeria, e ninguém deu por isso porque o registo saía na mesma.
#
# Sonda o motor dinâmico do clap com a MESMA forma que o script de registo usa
# (`COMPLETE=bash <bin> -- <palavras>`), e falha quando não vem candidato nenhum.
completa() {                      # $@ = a linha, com "" na posição a completar
  local n
  n=$(COMPLETE=bash _CLAP_COMPLETE_INDEX=$(( $# - 1 )) _CLAP_IFS=$'\n' \
      _CLAP_COMPLETE_SPACE=true "$BIN" -- "$@" 2>/dev/null | grep -vc '^-')
  [ "${n:-0}" -gt 0 ]
}
# Estes dois não dependem de estado nenhum do host: o `man` lê o catálogo de
# páginas, o `restore` é um caminho de ficheiro.
check "man completa nomes de comando" ok completa delonix man ""
check "system snapshot restore completa caminhos" ok completa delonix system snapshot restore ""
# Este só vale onde o recurso existe — zero num host sem imagens VM é a resposta
# honesta, não uma falha, e um SKIP declarado conta como NÃO COBERTO.
if [ "$("$BIN" image vm ls 2>/dev/null | tail -n +2 | wc -l)" -gt 0 ]; then
  check "image vm rm completa (o destrutivo)" ok completa delonix image vm rm ""
else
  skip "image vm rm completa" "não há imagens VM neste host"
fi

########################################
section "compatibility compose / migrate assess (M02)"
########################################
# A TABELA e o COMPORTAMENTO são lidos a um comando de distância, e a tabela é
# escrita à mão a partir de três listas. Se discordarem, é ela que mente —
# alguém lê `served` e leva um erro, ou lê `missing` e nunca tenta. Por isso o
# check não confere a tabela contra si própria: pega em cada chave que ela
# classifica e pergunta ao `compose config` o que ele faz com um ficheiro que a
# use. 88 perguntas, uma por chave.
COMPW=$(mktemp -d "${TMPDIR:-/tmp}/e2e-compat-XXXXXX")
check "compatibility compose imprime a matriz" ok bash -c \
  "'$BIN' compatibility compose | grep -q 'Compose Specification coverage'"
check "compatibility compose -o json parseia e traz as duas listas" ok bash -c \
  "'$BIN' compatibility compose -o json | python3 -c \"import json,sys; d=json.load(sys.stdin); assert d['service_keys'] and d['top_level_keys']\""
check "a tabela do compose descreve o que o compose faz, chave a chave" ok bash -c "
  mkdir -p '$COMPW/cross'
  '$BIN' compatibility compose -o json > '$COMPW/cross/m.json'
  python3 - '$BIN' '$COMPW/cross' <<'PYX'
import json, os, subprocess, sys
b, w = sys.argv[1], sys.argv[2]
m = json.load(open(f'{w}/m.json'))
bad = []
for row in m['service_keys']:
    key, state = row['key'], row['state']
    if key == 'image':
        continue
    open(f'{w}/compose.yaml', 'w').write(f'services:\n  web:\n    image: alpine\n    {key}: x\n')
    r = subprocess.run([b, 'compose', 'config'], capture_output=True, text=True, cwd=w)
    out = r.stderr + r.stdout
    # Classifica pela MENSAGEM e nao pelo rc: o valor de sonda e sempre `x`, e
    # uma chave SERVIDA que espera uma lista recusa por TIPO. Ler isso como
    # "nao servida" acusava 17 chaves servidas de nao o serem - o check estaria
    # a medir o meu valor de sonda, nao a tabela.
    not_served = any(f in out for f in ('is not read by', 'not understood', 'is not supported'))
    if (state == 'served') == not_served:
        bad.append((key, state, out.strip()[:80]))
if bad:
    print('a tabela discorda do compose em', len(bad), 'chave(s):', bad[:5])
    sys.exit(1)
PYX
"
cat >"$COMPW/docker-compose.yml" <<'YAML'
services:
  db:
    image: postgres:16
    environment: {POSTGRES_PASSWORD: dev}
    devices: ['/dev/null:/dev/null']
  web:
    image: nginx:alpine
    ports: ['8080:80']
YAML
check "migrate assess lê o ficheiro e classifica cada chave" ok bash -c \
  "cd '$COMPW' && '$BIN' migrate assess | grep -q 'devices' && '$BIN' migrate assess | grep -q 'container run --device'"
# O contrato de exit code é a única metade que um portão de CI consome.
check "migrate assess --detailed-exitcode devolve 2 com uma chave não servida" 2 \
  bash -c "cd '$COMPW' && '$BIN' migrate assess --detailed-exitcode"
cat >"$COMPW/limpo.yml" <<'YAML'
services:
  web:
    image: nginx:alpine
    ports: ['80']
YAML
check "migrate assess devolve 0 quando tudo é servido" 0 \
  "$BIN" migrate assess -f "$COMPW/limpo.yml" --detailed-exitcode
check "migrate assess -o json" ok bash -c \
  "'$BIN' migrate assess -f '$COMPW/limpo.yml' -o json | python3 -c \"import json,sys; d=json.load(sys.stdin); assert d['served']>0 and d['missing']==0\""
check "migrate assess de um ficheiro inexistente diz 4" 4 \
  "$BIN" migrate assess -f "$COMPW/naoexiste.yml"
rm -rf "$COMPW"

########################################
section "provider ls / describe / matrix (ADR-0050): a matriz medida, não afirmada"
# O que se prova aqui é a LIGAÇÃO: que o verbo lê as declarações reais, que o
# JSON leva os campos que o contrato (`ProviderInfo`) leva, que um `supported`
# nunca vem sem evidência, e que a matriz publicada é a gerada — o teste
# unitário do bin compara o ficheiro, este check confirma-o contra o binário.
check "provider ls" ok "$BIN" provider ls
check "provider ls -o json é um array com os 6 providers" ok bash -c \
  "'$BIN' provider ls -o json | python3 -c 'import json,sys; v=json.load(sys.stdin); assert len(v)==6, len(v)'"
check "provider ls -o json: cada capacidade leva name/supported/state/detail" ok bash -c \
  "'$BIN' provider ls -o json | python3 -c '
import json,sys
for p in json.load(sys.stdin):
    assert p[\"id\"] and p[\"kind\"] in (\"compute\",\"network\",\"storage\"), p
    assert p[\"health\"][\"reason\"], p[\"id\"]
    for c in p[\"capabilities\"]:
        assert set(c) >= {\"name\",\"supported\",\"state\",\"detail\",\"domain\"}, c
        assert c[\"state\"] != \"supported\" or c[\"detail\"], (p[\"id\"], c[\"name\"])
'"
check "provider ls --kind network só traz a rede" ok bash -c \
  "'$BIN' provider ls --kind network -o json | python3 -c 'import json,sys; v=json.load(sys.stdin); assert [p[\"kind\"] for p in v]==[\"network\"], v'"
check "provider describe libvirt" ok "$BIN" provider describe libvirt
check "provider describe linux --kind storage" ok "$BIN" provider describe linux --kind storage
check "provider describe de um provider inexistente diz 4" 4 "$BIN" provider describe naoexiste
check "provider ls --kind inválido recusa" fail "$BIN" provider ls --kind ceph
check "provider matrix é a matriz publicada, byte a byte" ok bash -c \
  "diff <('$BIN' provider matrix) '$(dirname "$0")/../docs/providers/capability-matrix.md' >/dev/null"
check "provider ls --l18n=pt traduz o cabeçalho" ok bash -c \
  "'$BIN' --l18n=pt provider ls | grep -q 'medidos neste host'"

section "erros: a CLI tem de RECUSAR o que é inválido"
########################################
check "container describe de inexistente recusa" fail "$BIN" container describe naoexiste-$PFX
check "container inspect de inexistente recusa" fail "$BIN" container inspect naoexiste-$PFX
check "volume inspect de inexistente recusa" fail "$BIN" volume inspect naoexiste-$PFX
check "network inspect de inexistente recusa" fail "$BIN" network inspect naoexiste-$PFX
check "container update sem mudanças recusa" fail "$BIN" container update naoexiste-$PFX
check "container stop de inexistente recusa" fail "$BIN" container stop naoexiste-$PFX
check "container rm de inexistente recusa" fail "$BIN" container rm naoexiste-$PFX
check "delete vm de inexistente recusa" fail "$BIN" delete vm naoexiste-$PFX
check "stack apply de ficheiro inexistente recusa" fail "$BIN" stack apply -f /nao/existe.yaml

########################################
section "códigos de saída: a CLASSE da falha, não só que falhou"
########################################
# Medido antes de isto existir: «não existe» e «rebentou» eram ambos 1, logo um
# reconciliador só os distinguia pela MENSAGEM — que é traduzida (`--l18n=pt`),
# portanto o script deixava de classificar num nó com outra locale. O mapa vive
# num sítio só (`cmd::exitcode`), mas a LIGAÇÃO (main.rs, `for_each_id`) só se
# prova aqui: um teste unitário do mapa passa na mesma com o `main` a ignorá-lo.
check "inexistente: container inspect diz 4" 4 "$BIN" container inspect naoexiste-$PFX
check "inexistente: volume inspect diz 4" 4 "$BIN" volume inspect naoexiste-$PFX
check "inexistente: network inspect diz 4" 4 "$BIN" network inspect naoexiste-$PFX
check "inexistente: secret rm diz 4" 4 "$BIN" secret rm naoexiste-$PFX
check "inexistente: delete vm diz 4" 4 "$BIN" delete vm naoexiste-$PFX
# O lote tem caminho de saída PRÓPRIO (`for_each_id` sai antes de o `main` ver o
# erro): sem a mesma classificação lá, `rm a b` respondia 1 onde `rm a` diz 4.
check "inexistente: lote de ids mantém a classe" 4 \
  "$BIN" container rm naoexiste1-$PFX naoexiste2-$PFX
# A classe não pode depender da língua — é essa a razão de existir do número.
check "inexistente em PT continua a dizer 4" 4 \
  "$BIN" --l18n=pt container inspect naoexiste-$PFX
# O número do dicionário (ADR-0043) na LINHA de erro, e na língua do operador: é
# o que um log, um ticket ou um screenshot levam. Um teste unitário do rótulo passa
# na mesma com o `main` a imprimir a mensagem sem ele.
check "a linha de erro traz o número do dicionário" ok \
  bash -c "\"$BIN\" --l18n=pt vm stop naoexiste-$PFX 2>&1 | grep -q '\[DX-4501\]'"
check "o lote de ids traz o número por id" ok \
  bash -c "\"$BIN\" container rm naoexiste1-$PFX naoexiste2-$PFX 2>&1 | grep -c '\[DX-4101\]' | grep -qx 2"
check "explain de um código responde" ok "$BIN" explain DX-4501
check "explain de um código que não existe diz 4" 4 "$BIN" explain DX-4299
check "explain codes --json é JSON" ok \
  bash -c "\"$BIN\" explain codes --json | python3 -c 'import json,sys; assert len(json.load(sys.stdin)) > 10'"
# --- as duas classes novas ---
# As duas só entraram porque tinham PRODUTORES reais mal classificados: as duas
# respondiam `1`, o mesmo número de um apply rebentado. E a ligação só se prova
# aqui — o mapa em `cmd::exitcode` passa nos testes na mesma com o `main` a
# ignorá-lo, que é a razão de esta secção existir.
#
# 124 é o do `timeout(1)`: um prazo esgotado não é uma falha, e um reconciliador
# que o leia como «rebentou» recria um recurso que estava a subir.
E2E_WAITMF=$(mktemp "${TMPDIR:-/tmp}/e2e-wait-XXXXXX.yaml")
cat > "$E2E_WAITMF" <<YAML
apiVersion: delonix.io/v1
kind: Container
metadata: { name: naovaisubir-$PFX }
spec:
  image: naoexiste.invalid/naoexiste:0
YAML
check "prazo esgotado no stack wait diz 124" 124 \
  "$BIN" stack wait -f "$E2E_WAITMF" --timeout 1
rm -f "$E2E_WAITMF"

# 69 é o `EX_UNAVAILABLE` do sysexits.h. Só é exercitável num host a que falte
# mesmo a ferramenta — com ela instalada o caminho não existe e o honesto é
# SKIP, nunca um verde que não correu nada (a mesma regra do bloco do `wg`).
if command -v virt-customize >/dev/null 2>&1; then
  skip "ferramenta em falta diz 69" "este host TEM virt-customize — o caminho não é exercitável aqui"
else
  check "ferramenta em falta diz 69" 69 \
    "$BIN" image vm build --no-k8s --distro ubuntu --ubuntu-release 24.04 -t e2e-nao-$PFX
fi

# Convenções instaladas que NÃO podem ter mudado.
check "uso inválido continua a ser o 2 do clap" 2 "$BIN" subcomando-que-nao-existe
check "sucesso continua a ser 0" 0 "$BIN" container ps

########################################
section "volumes: ciclo de vida"
########################################
# `volumes` (plural) já não existe — renomeado para `volume` sem alias no B2.
# Corrigido aqui de caminho: `set -e` faria QUALQUER destas linhas abortar o
# script inteiro, e é por isso que nada da secção seguinte (share volumes,
# fundida em `volume` no B5) alguma vez chegava a correr.
VOL="vol-$PFX"
check "volume create" ok "$BIN" volume create "$VOL"
check "volume create idempotente" ok "$BIN" volume create "$VOL"
check "volume ls mostra-o" ok bash -c "'$BIN' volume ls | grep -q '$VOL'"
check "volume inspect" ok "$BIN" volume inspect "$VOL"
check "volume describe" ok "$BIN" volume describe "$VOL"
# Conflito (5): «o nome já está tomado» é uma resposta diferente de «o argumento
# está errado» — quem reconcilia adopta/salta no primeiro caso e pára no segundo.
check "snapshot create" ok "$BIN" volume snapshot create "$VOL" --name s1
check "snapshot já existente diz 5 (conflito)" 5 "$BIN" volume snapshot create "$VOL" --name s1
check "snapshot rm" ok "$BIN" volume snapshot rm "$VOL" s1

########################################
section "volume create: --driver/--opt (Sprint 6 — fusão do --type/--server/--share)"
########################################
# `storage`/`sharevolume` tinham ZERO checks executados — o balde dos
# «comandos nunca executados» — e o próprio grupo `storage` já não existe.
# O `--type`/`--server`/`--share`/`--device`/`--options` da fusão B5 tinha DUAS
# formas para a mesma informação (a amigável e a crua); o Sprint 6 fecha-as
# numa só, ao estilo `docker volume create --driver <nome> --opt k=v`.
STGN="stg-$PFX"
check "--type já não existe (corte limpo, sem alias)" 2 \
  "$BIN" volume create "$STGN" --type nfs --server 10.99.99.99 --share /x
check "volume create --driver desconhecido recusa" 1 \
  "$BIN" volume create "$STGN" --driver naoexiste --opt server=10.99.99.99 --opt share=/x
check "volume create --driver nfs sem --opt share recusa" 1 \
  "$BIN" volume create "$STGN" --driver nfs --opt server=10.99.99.99
check "--opt com --driver local (que não leva opções) recusa" 1 \
  "$BIN" volume create "$STGN" --driver local --opt server=10.99.99.99
check "--opt com chave desconhecida recusa" 1 \
  "$BIN" volume create "$STGN" --driver nfs --opt server=10.99.99.99 --opt share=/x --opt bogus=1
check "--driver e --parent são exclusivos" 2 \
  "$BIN" volume create "$STGN" --driver nfs --opt server=10.99.99.99 --opt share=/x --parent "$VOL"
if "$BIN" volume create "$STGN" --driver nfs --opt server=10.99.99.99 --opt share=/exports/x >/dev/null 2>&1; then
  check "volume ls mostra-o" ok bash -c "'$BIN' volume ls | grep -q '$STGN'"
  "$BIN" volume rm -f "$STGN" >/dev/null 2>&1
else
  skip "volume create --driver nfs com NAS real" "montar NFS/CIFS exige CAP_SYS_ADMIN — não exercitável em rootless"
  check "um create falhado não deixa registo em volume ls" ok bash -c \
    "! '$BIN' volume ls 2>/dev/null | grep -q '$STGN'"
fi

########################################
section "share volumes (kind: Volume com bloco share:)"
########################################
# O grupo `sharevolume` tinha ZERO checks — o balde dos «comandos nunca
# executados» a pagar-se outra vez, e logo no caminho que acabou de mudar de
# forma: um share deixou de ser `kind: ShareVolume` e passou a ser um
# `kind: Volume` com bloco `share:`, com o registo antigo absorvido pelo volume.
# A v0.65.0 fechou o degrau — a grafia antiga já não é reescrita com um aviso,
# é RECUSADA, e o que se exercita abaixo é a recusa.
#
# O que prova a fusão é o CICLO, não um comando isolado: cada passo devolve 0
# sozinho mesmo com a posse partida. As asserções que valem são três — o plano
# tratar dois shares homónimos como recursos DISTINTOS, o `destroy` alcançá-los
# (o que era impossível quando um share não era possuível), e os dados do
# inquilino continuarem no disco depois disso.
SHWORK="$OUT/share-$PFX"; mkdir -p "$SHWORK"
SHPAI="shpai-$PFX"
check "volume pai para os shares" ok "$BIN" volume create "$SHPAI"
cat >"$SHWORK/shares.yaml" <<YAML
apiVersion: delonix.io/v1
kind: Volume
metadata:
  name: sh-$PFX
  namespace: shteam-a
spec:
  share:
    storageRef: $SHPAI
  quota: 5G
  alertPct: 80
---
apiVersion: delonix.io/v1
kind: Volume
metadata:
  name: sh-$PFX
  namespace: shteam-b
spec:
  share:
    from: $SHPAI
  quota: 2G
YAML
# As duas escritas do PAI no mesmo ficheiro, de propósito: saiu o Kind, não o
# campo — `share.storageRef` é a grafia que o `kind: ShareVolume` usava e
# continua a ser um alias de `share.from`, para que um manifesto que só renomeie
# o Kind não tenha de renomear também o campo. É a promessa que o
# examples/sharevolume.yaml faz por escrito, e ninguém a exercitava.
#
# Dentro do bloco `share:`, e não em `spec.storageRef` — medido: um
# `spec.storageRef` solto é campo DESCONHECIDO, sai um WARNING e o apply devolve
# 0 tendo criado um volume local vulgar. Uma renomeação mecânica do Kind cai
# exactamente nesse buraco, que é a razão de a escrita antiga ser exercitada
# aqui em vez de se assumir que ainda funciona.
check "apply das duas escritas do pai (storageRef + share.from)" ok \
  "$BIN" volume apply -f "$SHWORK/shares.yaml"
# A v0.65.0 REMOVEU `kind: ShareVolume`. O que aqui se prova não é que recusa —
# um «unknown kind» genérico também recusaria — é que a recusa NOMEIA o que
# escrever em vez dele: sem isso, quem apanha um manifesto correcto-até-ontem
# fica sem saber se escreveu mal ou se algo mudou debaixo dele.
cat >"$SHWORK/velho.yaml" <<YAML
apiVersion: delonix.io/v1
kind: ShareVolume
metadata:
  name: shvelho-$PFX
  namespace: shteam-a
spec:
  storageRef: $SHPAI
  quota: 5G
YAML
check "kind: ShareVolume é recusado" fail \
  "$BIN" volume apply -f "$SHWORK/velho.yaml"
# Sem backticks no padrão de propósito: o texto do erro tem-nos, e dentro das
# aspas duplas deste `bash -c` seriam substituição de comandos. Os `.` cobrem-nos.
check "a recusa nomeia a forma nova" ok \
  bash -c "'$BIN' volume apply -f '$SHWORK/velho.yaml' 2>&1 | grep -q 'kind: Volume. with a .share:. block'"
# `sharevolume ls` fundiu-se em `volume ls -A` (B5 CLI collapse — todas as
# namespaces de uma vez, o que `sharevolume ls` sempre fazia por omissão).
check "volume ls -A mostra os dois" ok \
  bash -c "test \$('$BIN' volume ls -A | grep -c 'sh-$PFX') -eq 2"
# O guarda do `volume rm` do PAI, contra um share possuído por uma NAMESPACE.
# Medido vivo na v3.0.0: dava rc=0 sem `--force` e sem uma linha de recusa — o
# `volume_refs` procurava os shares com `store.list()`, que por desenho não vê a
# sub-árvore `volumes/.ns/<ns>/`, e o pai era destruído por baixo dos inquilinos,
# que ficavam como registo a apontar para uma árvore apagada. A metade NÃO
# namespaced já estava coberta; esta é a que faltava, e é a que um cliente real
# usa, porque um share existe precisamente para ter dono.
check "volume rm do pai é RECUSADO por um share namespaced" fail \
  "$BIN" volume rm "$SHPAI"
# Recusar não chega: a recusa tem de dizer QUEM segura o pai, e com a namespace
# ao lado do nome. Dois inquilinos aqui têm um share com o MESMO nome (`sh-$PFX`
# em shteam-a e em shteam-b) — um erro que dissesse só `sh-$PFX` não diria a qual
# dos dois donos ir falar. A grafia é a `<ns>/<nome>` que o plano já usa.
check "a recusa nomeia os shares com a namespace" ok \
  bash -c "'$BIN' volume rm '$SHPAI' 2>&1 | grep -q 'shteam-a/sh-$PFX' && '$BIN' volume rm '$SHPAI' 2>&1 | grep -q 'shteam-b/sh-$PFX'"
# E a recusa é um guarda, não um bloqueio: o `--force` continua a ser o caminho
# escrito no próprio texto do erro. Sem esta terceira asserção, um `rm` que
# recusasse SEMPRE passava as duas de cima.
check "o pai continua de pé depois das duas recusas" ok \
  bash -c "'$BIN' volume ls | grep -q '$SHPAI'"
# Dois inquilinos com o MESMO nome de share: o reconciliador identifica por
# (kind, nome), por isso sem qualificar a namespace os dois seriam UM recurso —
# um apareceria como deriva do outro em todos os planos, e um `--replace` levava
# ambos. O nome no plano é `<ns>/<nome>`.
check "o plano distingue os dois shares homónimos" ok \
  bash -c "'$BIN' stack plan -f '$SHWORK/shares.yaml' | grep -q 'Volume/shteam-a/sh-$PFX' && '$BIN' stack plan -f '$SHWORK/shares.yaml' | grep -q 'Volume/shteam-b/sh-$PFX'"
check "stack apply adopta e carimba a posse" ok \
  "$BIN" stack apply -f "$SHWORK/shares.yaml"
# O que a fusão existe para dar: um share possuível. Sem isto o plano proporia
# `Adopt` para sempre — deriva eterna, e o `--prune`/`destroy` nunca lhe chegava.
check "manifesto inalterado propõe ZERO alterações" ok \
  "$BIN" stack plan -f "$SHWORK/shares.yaml" --detailed-exitcode
# Um ficheiro do inquilino, para o destroy ter alguma coisa que possa destruir
# por engano. `sharevolume describe -n <ns>` fundiu-se em `volume describe -n <ns>`.
SHDATA="$("$BIN" volume describe "sh-$PFX" -n shteam-a | awk '/Mountpoint/{print $2}')"
[[ -n "$SHDATA" ]] && echo "dados-do-inquilino" >"$SHDATA/ficheiro.txt"
check "o describe do share resolveu um mountpoint" ok test -n "$SHDATA"
check "stack destroy alcança os dois shares" ok \
  "$BIN" stack destroy -f "$SHWORK/shares.yaml"
check "o destroy tirou-os do registo" ok \
  bash -c "test \$('$BIN' volume ls -A | grep -c 'sh-$PFX') -eq 0"
# A garantia que o `remove_with` sempre deu (nunca toca num mountpoint externo)
# e que ninguém tinha exercitado pelo caminho declarativo.
check "os DADOS do inquilino sobreviveram ao destroy" ok test -f "$SHDATA/ficheiro.txt"
# Um share não monta nada — declarar um mount ao lado é dois volumes num
# documento, e honrar um deles em silêncio é a falha que isto recusa.
cat >"$SHWORK/mau.yaml" <<YAML
apiVersion: delonix.io/v1
kind: Volume
metadata:
  name: shmau-$PFX
spec:
  share:
    from: $SHPAI
  nfs:
    server: 10.0.0.1
    share: /export
YAML
check "share + nfs no mesmo volume é recusado" fail \
  "$BIN" volume apply -f "$SHWORK/mau.yaml"
"$BIN" volume rm -f "$SHPAI" >/dev/null 2>&1

########################################
section "volume create --parent (fusão B5 — antigo sharevolume) imperativo"
########################################
# O caminho MANIFESTO acima já prova a fusão de ponta a ponta; isto prova só
# o caminho IMPERATIVO novo, que não existia antes desta fatia — criar uma
# share sem escrever um manifesto.
SHPARENT="shparent-$PFX"
check "volume pai" ok "$BIN" volume create "$SHPARENT"
check "volume create --parent" ok "$BIN" volume create "shchild-$PFX" --parent "$SHPARENT" --quota 1M
# ACH-001 (v3.0.0, ALTA): o `create` sem `--namespace` escrevia em `.ns/default/`
# e os tres comandos de leitura SEM flag liam a raiz. Medido: `create` devolvia 0
# com a share no disco a consumir quota do pai, e `describe`/`rm` respondiam
# `no such volume` (rc=4) sobre uma coisa que existia. Os tres checks abaixo sao
# um so invariante — **o que se escreve sem flag le-se sem flag** — e por isso
# nao passa nenhuma: uma flag num deles apagava exactamente o que se quer provar.
check "ACH-001: a share sem --namespace aparece no ls SEM flag, com PARENT" ok \
  bash -c "'$BIN' volume ls | awk '{print \$1\" \"\$3}' | grep -q 'shchild-$PFX $SHPARENT'"
check "ACH-001: o describe SEM flag encontra-a" ok \
  "$BIN" volume describe "shchild-$PFX"
# O guarda que a invisibilidade contornava: com a share fora do alcance do
# `volume_refs` do pai, o `rm` do PAI passava sem `--force` e sem uma palavra
# sobre o que ficava pendurado — o modo de falha que o proprio codigo nomeia.
check "ACH-001: o rm do PAI recusa por causa dela (o guarda ve-a)" 1 \
  "$BIN" volume rm "$SHPARENT"
check "--purge-data é recusado num volume sem --parent" 1 \
  "$BIN" volume rm "$SHPARENT" --purge-data
SHCDATA="$("$BIN" volume describe "shchild-$PFX" | awk '/Mountpoint/{print $2}')"
[[ -n "$SHCDATA" ]] && echo "dados" >"$SHCDATA/f.txt"
check "ACH-001: o rm SEM flag encontra-a (e sem --purge-data preserva os dados)" ok \
  "$BIN" volume rm "shchild-$PFX"
check "…e os dados sobreviveram" ok test -f "$SHCDATA/f.txt"
# Recriar para provar o --purge-data em separado (o rm anterior já desregistou).
check "volume create --parent outra vez" ok "$BIN" volume create "shchild-$PFX" --parent "$SHPARENT"
SHCDATA2="$("$BIN" volume describe "shchild-$PFX" | awk '/Mountpoint/{print $2}')"
check "rm --purge-data apaga os dados" ok "$BIN" volume rm "shchild-$PFX" --purge-data
check "…e o subdirectório já não existe" fail test -d "$SHCDATA2"
"$BIN" volume rm -f "$SHPARENT" >/dev/null 2>&1

########################################
section "network: ciclo de vida"
########################################
NET="net-$PFX"
# A subnet abaixo é FIXA, e este script já foi interrompido a meio (um `timeout`
# do lado de quem o corre): a rede fica para trás a segurar `10.253.0.0/16` e a
# corrida seguinte falha em `network create` com um conflito que nada tem a ver
# com o código — mais quatro falhas em cascata. Varrer o que só este script cria
# (`net-e2e*`) antes de começar é o que torna a bateria repetível.
for stale in $("$BIN" network ls 2>/dev/null | awk '/^net-e2e/{print $1}'); do
  [[ "$stale" == "$NET" ]] && continue
  "$BIN" network rm "$stale" >/dev/null 2>&1 && log "  (limpo: rede $stale de uma corrida anterior)"
done
# `/16` na gama 10.<200-254>, e não o `/24` que este teste usou durante meses: até
# a v0.48.0 o `--subnet` era ACEITE e deitado fora com o driver bridge, por isso um
# `/24` "passava" sem nunca ter sido aplicado. Fechado o bug, o teste que o
# codificava passou a falhar — a armadilha «um teste pode codificar o bug» que este
# repo já tinha catalogado com o `default_project_name` do compose. Octetos altos
# de propósito, para não colidirem com uma rede real do host.
check "network create" ok "$BIN" network create "$NET" --subnet 10.253.0.0/16
check "network ls mostra-a" ok bash -c "'$BIN' network ls | grep -q '$NET'"
check "network inspect" ok "$BIN" network inspect "$NET"
check "network describe" ok "$BIN" network describe "$NET"

# --- `network diagnose`: o estado VIVO, não a capacidade do host ----------
# A pergunta que faltava. O `system doctor` responde «este host CONSEGUE?»
# (br_netfilter, delegação de cgroup) — capacidade estática. Este responde «a
# rede que ESTÁ aqui é coerente?». Não se repetem de propósito: dois comandos a
# verificar o br_netfilter são duas respostas que começam a discordar.
check "network diagnose responde" ok "$BIN" network diagnose
check "network diagnose cobre as três perguntas" ok bash -c \
  "'$BIN' network diagnose -o json | grep -q 'control plane' && \
   '$BIN' network diagnose -o json | grep -q 'networks' && \
   '$BIN' network diagnose -o json | grep -q 'address registry'"
check "network diagnose -o json é JSON válido" ok bash -c \
  "'$BIN' network diagnose -o json | python3 -c 'import json,sys; json.load(sys.stdin)'"
# O `status` é o campo que um health check lê, e tem de ser uma das três
# palavras — não um booleano: «sem plano de controlo» e «nenhuma rede realizada»
# são respostas diferentes.
check "cada linha tem um status conhecido" ok bash -c \
  "'$BIN' network diagnose -o json | python3 -c \"
import json,sys
d=json.load(sys.stdin)
assert d, 'sem linhas'
assert all(x['status'] in ('ok','warn','down') for x in d), d
\""
# NÃO ceifa. É a garantia que separa este comando de um `prune`: mostrar um
# lease é seguro, reclamá-lo não — um container tem lease ANTES de ter registo.
check "diagnose não mexe no registo de endereços" ok bash -c \
  "before=\$(find \"\${DELONIX_ROOT:-\$HOME/.local/share/delonix}/ipam\" -name '*.json' -newermt '-1 second' 2>/dev/null | wc -l)
   '$BIN' network diagnose >/dev/null 2>&1
   after=\$(find \"\${DELONIX_ROOT:-\$HOME/.local/share/delonix}/ipam\" -name '*.json' -newermt '-1 second' 2>/dev/null | wc -l)
   [ \"\$before\" = \"\$after\" ]"

# --- `network route` sem argumentos LISTA (B-1) ---------------------------
# As rotas eram persistidas (`ingress/routes/<from>--<to>.json`) e enumeráveis
# (`infra::route_list` é público), e NADA as mostrava: dava para abrir uma
# excepção ao isolamento entre redes e depois não havia comando que a visse.
# Estes checks EXECUTAM — a bateria já verificava o `--help` de 100% das folhas
# e corria um quarto delas, e foi em folhas nunca executadas que os dois
# achados de `net` desta série apareceram.
check "network route sem argumentos lista" ok "$BIN" network route
check "network route -o json é JSON a sério" ok bash -c \
  "'$BIN' network route -o json | python3 -c 'import json,sys; json.load(sys.stdin)'"
# Um argumento só: a identidade de uma rota é o PAR. Adivinhar qual das pontas
# faltava seria escolher por quem escreveu.
check "network route com um argumento só recusa" fail "$BIN" network route "$NET"
check "a recusa NOMEIA a forma que funciona" ok bash -c \
  "'$BIN' network route '$NET' 2>&1 | grep -q 'PAIR'"

NET2="net2-e2e-$PFX"
if "$BIN" network create "$NET2" --subnet 10.252.0.0/16 >/dev/null 2>&1 \
   && "$BIN" network route "$NET" "$NET2" >/dev/null 2>&1; then
  # O CICLO, que é a única coisa que prova alguma coisa aqui: cada passo
  # isolado devolve 0 mesmo com a listagem partida.
  check "a rota aberta aparece na listagem" ok bash -c \
    "'$BIN' network route | grep -q '$NET2'"
  check "e aparece no json" ok bash -c \
    "'$BIN' network route -o json | python3 -c \"import json,sys; sys.exit(0 if any(r['to']=='$NET2' for r in json.load(sys.stdin)) else 1)\""
  # «não consegui perguntar ao holder» NUNCA se lê como «a rota está fechada» —
  # o `@netpair` vive na netns efémera e num nó ocioso está vazio.
  check "o estado nunca diz 'closed'" ok bash -c \
    "! '$BIN' network route | grep -qi 'closed'"
  "$BIN" network route "$NET" "$NET2" --rm >/dev/null 2>&1 || true
  check "fechada, sai da listagem" ok bash -c \
    "! '$BIN' network route | grep -q '$NET2'"
  "$BIN" network rm "$NET2" >/dev/null 2>&1 || true
else
  "$BIN" network rm "$NET2" >/dev/null 2>&1 || true
  skip "ciclo de uma rota" "não foi possível criar a 2.ª rede ou abrir a rota neste host"
fi

# --- `--gateway` declarado: validado SEMPRE, e relatado como está em vigor ---
# Um teste unitário do validador não prova nada aqui, porque o bug não estava no
# validador: estava na LIGAÇÃO. A chamada vivia dentro do braço `Some(--subnet)`
# do `match` enquanto o valor era consumido fora dele, por isso um `--gateway`
# sem `--subnet` chegava ao registo sem uma única verificação — o `create` dizia
# sucesso e o primeiro `attach` morria num `ip route add default via` para um
# endereço fora da rede. É por isso que o primeiro check NÃO passa `--subnet`.
check "gateway fora do prefixo: recusa mesmo SEM --subnet" fail \
  "$BIN" network create "gwx-$PFX" --gateway 8.8.8.8
check "gateway recusado não deixa registo órfão" fail bash -c \
  "'$BIN' network ls | grep -q 'gwx-$PFX'"
check "gateway = endereço de rede: recusa" fail \
  "$BIN" network create "gwn-$PFX" --subnet 10.251.0.0/16 --gateway 10.251.0.0
NETGW="netgw-$PFX"
if "$BIN" network create "$NETGW" --subnet 10.251.0.0/16 --gateway 10.251.0.254 >/dev/null 2>&1; then
  # O que se lê no `inspect` tem de ser o que os workloads recebem. O registo
  # declarativo DERIVA sempre `.0.1`; sem ler o plano físico, esta linha dizia
  # um endereço e o dataplane usava outro — a mesma família do nome de bridge
  # que a CLI imprimia sem existir no host.
  check "inspect relata o gateway EM VIGOR, não o derivado" ok bash -c \
    "'$BIN' network inspect '$NETGW' | grep -q '10.251.0.254'"
  check "describe relata o gateway EM VIGOR" ok bash -c \
    "'$BIN' network describe '$NETGW' | grep -q '10.251.0.254'"
  "$BIN" network rm "$NETGW" >/dev/null 2>&1 || true
else
  skip "gateway declarado: relato" "network create --gateway falhou neste host"
fi

# --- `wg` ausente: nem errno cru, nem sucesso falso ---
# Dois achados de uma varredura de auditoria, os dois neste grupo, os dois em
# comandos que a bateria nunca EXECUTAVA (só lhes verificava o `--help`):
#   1. `node init|key` devolvia `spawn failed: No such file or directory`. O
#      ENOENT de um spawn não é um ficheiro em falta — é a FERRAMENTA; a frase
#      manda o leitor procurar um caminho. Mesma classe que o `vmimage::
#      tool_package` já corrigira, reaparecida noutro sítio.
#   2. `create --driver overlay --wg-ip` saía **0** com a rede POR REALIZAR, e
#      prometia reconciliar «no próximo create» — o que `create_overlay` não faz:
#      o retry dava conflito (5) e a rede ficava sem comando que a salvasse.
# O gate testa o COMPORTAMENTO, não o ambiente: num host com `wg` o caminho de
# falha não existe e o honesto é SKIP, nunca um verde que não exercitou nada.
if command -v wg >/dev/null 2>&1; then
  skip "wg ausente: caminho de falha" "este host TEM wg — o caminho não é exercitável aqui"
else
  # Era `1` até os códigos ganharem a classe «capacidade que este host não
  # tem»: indistinguível de um erro de escrita numa flag, quando a acção
  # seguinte é oposta (instalar wireguard-tools, não corrigir o comando).
  check "node key sem wg: recusa (classe 69)" 69 "$BIN" network node key
  check "node key sem wg: nomeia a ferramenta, não o errno" ok bash -c \
    "o=\$('$BIN' network node key 2>&1); grep -q wireguard-tools <<<\"\$o\" && ! grep -q 'No such file' <<<\"\$o\""
  check "overlay cifrado sem wg: recusa em vez de sair 0" fail \
    "$BIN" network create "wgx-$PFX" --driver overlay --vni 99 --wg-ip 10.9.0.1/24
  check "overlay recusado não deixa registo órfão" fail bash -c \
    "'$BIN' network ls | grep -q 'wgx-$PFX'"
fi

########################################
section "image"
########################################
if [[ $E2E_ISOLATED -eq 1 ]]; then
  log "  (root isolado: $DELONIX_ROOT · runtime: $DELONIX_NET_RUNTIME_DIR)"
else
  log "  (E2E_SHARED_STATE=1 — ESTADO REAL da máquina)"
fi

IMG="${E2E_IMAGE:-alpine:3.19}"
# A guarda tem de procurar a REFERÊNCIA inteira, não só o repositório: com um
# `alpine:latest` no store e `alpine:3.19` ausente, o `grep alpine` passava e o
# `image describe alpine:3.19` a seguir falhava. `redis:7-alpine` também casava
# com `alpine`, o que tornava o falso positivo ainda mais fácil.
E2E_HAVE_IMAGE=1
# A saída é capturada ANTES do grep, aqui e nos guardas de imagem mais abaixo. Com
# `pipefail`, `image ls | grep -q` é uma corrida: o `grep -q` sai na primeira linha
# que casa, o `image ls` leva EPIPE na escrita seguinte e sai 141, e o `if` dá falso
# com a imagem no store — medido (duas secções saltadas em duas corridas seguidas).
_ils="$("$BIN" image ls 2>&1)"
if grep -qF "$IMG " <<<"$_ils"; then
  check "image describe" ok "$BIN" image describe "$IMG"
elif "$BIN" image pull "$IMG" >/dev/null 2>&1; then
  check "image pull ($IMG)" ok "$BIN" image ls
  check "image describe" ok "$BIN" image describe "$IMG"
else
  # Um root isolado começa vazio; sem rede o pull não acontece. Isto SALTA com a
  # razão em vez de chumbar — uma bateria vermelha por falta de rede esconde as
  # falhas verdadeiras, e um SKIP declarado conta como NÃO COBERTO, que é o que é.
  E2E_HAVE_IMAGE=0
  skip "image pull ($IMG)" "sem rede (ou registo inalcançável) e a imagem não está no store"
  skip "tudo o que precisa de $IMG" "a imagem não pôde ser obtida — ver o skip acima"
fi

# `image load` é o verbo que qualquer pessoa lê como ADITIVO — trazer um
# arquivo para dentro. Até 2026-09-10 substituía a lista de nomes da imagem:
# um store com `alpine:3.19` e um segundo nome para o MESMO id, mais um
# `load` do `save` do segundo, ficava só com o segundo. O primeiro nome
# desaparecia, e com ele o que um manifesto ou um compose lhe chamasse — num
# nó offline, um `run` que deixa de resolver a sua imagem.
if [[ $E2E_HAVE_IMAGE -eq 1 ]]; then
  TAGX="e2e-load-$PFX:v1"
  check "image load: um arquivo não deita fora os outros nomes da mesma imagem" ok bash -c "
    '$BIN' image tag '$IMG' '$TAGX' >/dev/null 2>&1 || exit 1
    '$BIN' image save '$TAGX' -o '$OUT/loadtags.tar' >/dev/null 2>&1 || exit 1
    '$BIN' image load -i '$OUT/loadtags.tar' >/dev/null 2>&1 || exit 1
    '$BIN' image ls | grep -qF '$IMG ' || { echo 'o nome original desapareceu no load'; '$BIN' image ls; exit 1; }
    '$BIN' image ls | grep -qF '$TAGX' || { echo 'o nome do arquivo não ficou'; exit 1; }"
  "$BIN" image remove "$TAGX" >/dev/null 2>&1
  rm -f "$OUT/loadtags.tar"
fi

########################################
section "container: ciclo de vida + hot reconfig"
########################################
C="c-$PFX"
# Porta alta e improvável de colidir com o que já corre na máquina.
P1=$((29500 + RANDOM % 300)); P2=$((29900 + RANDOM % 90))

# Um passo que se ANUNCIA tem de se FECHAR, e o caminho rápido não anuncia nada.
#
# O `Progress` com limiar (`step_after`, usado pelo `container run` sobre o
# desempacotar da imagem) imprimia o `•` de imediato fora de um TTY, enquanto o
# fecho é suprimido abaixo do limiar — na premissa de que «abaixo do limiar nada
# se anunciou», verdadeira num TTY (onde o spinner espera o limiar) e falsa aqui.
# Com a imagem já em cache: um `•`, zero `✓`. Ou seja, em CI, num pipe ou em
# qualquer redirecção ficava uma linha «em curso» sobre trabalho já terminado —
# exactamente onde é menos provável que alguém repare e mais provável que seja
# lida mais tarde. Só um TTY-menos o apanha, que é o que esta bateria é.
check "progresso: todo o • tem o seu ✓" ok bash -c "
  err=\$('$BIN' container run --rm '$IMG' true 2>&1 >/dev/null)
  o=\$(printf '%s\n' \"\$err\" | grep -c '•' || true)
  c=\$(printf '%s\n' \"\$err\" | grep -c '✓' || true)
  [ \"\$o\" = \"\$c\" ] || { printf 'abertos=%s fechados=%s\n%s\n' \"\$o\" \"\$c\" \"\$err\"; exit 1; }
"

check "container run -d -p" ok "$BIN" container run -d --name "$C" -p "$P1:80" "$IMG" sleep 600
if "$BIN" container inspect "$C" >/dev/null 2>&1; then
  check "container ls mostra-o" ok bash -c "'$BIN' container ls | grep -q '$C'"
  check "container describe" ok "$BIN" container describe "$C"
  check "container inspect (JSON válido)" ok bash -c "'$BIN' container inspect '$C' | python3 -m json.tool >/dev/null"
  check "container exec" ok "$BIN" container exec "$C" /bin/true
  check "container logs" ok "$BIN" container logs "$C"
  check "container stats" ok "$BIN" container stats "$C"

  # --- HOT RECONFIG: o núcleo desta sessão ---
  check "update: publish-add a quente" ok "$BIN" container update "$C" --publish-add "$P2:80"
  check "update: porta nova no registo" ok bash -c "'$BIN' container inspect '$C' | grep -q '$P2:80'"
  check "update: publish-add duplicado recusa" fail "$BIN" container update "$C" --publish-add "$P2:81"
  check "update: publish-rm a quente" ok "$BIN" container update "$C" --publish-rm "$P2"
  check "update: porta saiu do registo" fail bash -c "'$BIN' container inspect '$C' | grep -q '$P2:80'"
  check "update: publish-rm de porta não publicada recusa" fail "$BIN" container update "$C" --publish-rm 65001

  check "update: volume-add a quente" ok "$BIN" container update "$C" --volume-add "$VOL:/mnt/e2e"
  check "update: mount visível DENTRO do container" ok "$BIN" container exec "$C" /bin/sh -c "test -d /mnt/e2e"
  check "update: mount no registo" ok bash -c "'$BIN' container describe '$C' | grep -q '/mnt/e2e'"
  check "update: volume-add no mesmo destino recusa" fail "$BIN" container update "$C" --volume-add "$VOL:/mnt/e2e"
  check "update: volume-rm a quente" ok "$BIN" container update "$C" --volume-rm /mnt/e2e
  check "update: mount desapareceu de dentro" fail "$BIN" container exec "$C" /bin/sh -c "mountpoint -q /mnt/e2e"

  check "update: PID intacto após o hot reconfig" ok bash -c "test \"\$('$BIN' container inspect '$C' | python3 -c 'import json,sys; print(json.load(sys.stdin)[0][\"pid\"])')\" != 'None'"

  # A JANELA ENTRE O `run -d` E O PRIMEIRO `exec` (medida 2026-08-28).
  #
  # O `run -d` devolvia assim que libertava o init; o init só DEPOIS fazia o
  # `pivot_root` e montava os volumes. Um `exec` que apanhasse essa janela
  # entrava no mount namespace enquanto `/` ainda era o do HOST — corria o
  # `/bin/sh` do host e escrevia ficheiros do host, com exit 0. Silencioso
  # sempre que o caminho existe dos dois lados, e foi assim que passou dias por
  # flakiness: um backup tirado ali arquivava um volume vazio e a falha só
  # aparecia dois passos à frente, no restore, a apontar para o comando errado.
  #
  # A prova não pode ser o rc do `exec` — foi um rc=0 que escondeu isto. É um
  # caminho que existe SÓ no host: se o container o vê, o exec aterrou fora dele.
  #
  # POR QUE É QUE GERA CARGA, e o que isso admite. Isto é AMOSTRAGEM, não uma
  # verificação determinística: o que decide a corrida é o filho ser escalonado
  # antes de o `exec` (um processo novo) chegar ao `setns`, logo a taxa segue a
  # contenção de CPU do host e não o número de mounts. Medido no mesmo binário
  # defeituoso: 0/20 com a máquina folgada, 0/20 com 40 volumes, e 4–7/20 (20–35%)
  # com `nproc` workers a queimar CPU. Daí os 12 ciclos COM carga: entre 93% e
  # 99,8% de apanhar o defeito, contra praticamente 0% sem ela. Custa ~10s de
  # máquina saturada, e é o preço de o check não ser um verde por sorte.
  #
  # O que ele NÃO prova — que a espera existe e é limitada — está provado onde é
  # determinístico: `the_mount_wait_has_three_exits_and_none_is_unbounded`,
  # em `crates/adapters/delonix-linux/src/lib.rs`. Nenhum dos dois substitui o outro.
  check "run -d devolve com os mounts de pé (sem janela para o host)" ok bash -c "
    marca='$OUT/.so-existe-no-host'; : > \"\$marca\"
    carga=(); trap 'kill \"\${carga[@]}\" 2>/dev/null' EXIT
    for _ in \$(seq \$(nproc)); do ( while :; do :; done ) & carga+=(\$!); done
    for i in \$(seq 12); do
      n='rd-$PFX'-\$i
      '$BIN' container run -d --name \"\$n\" '$IMG' sleep 30 >/dev/null 2>&1
      onde=\$('$BIN' container exec \"\$n\" /bin/sh -c \"test -e '\$marca' && echo HOST || echo CONTAINER\" 2>&1)
      '$BIN' container rm -f \"\$n\" >/dev/null 2>&1
      case \"\$onde\" in
        *CONTAINER*) ;;
        *) printf 'ciclo %s: o exec aterrou fora do container (%s)\n' \"\$i\" \"\$onde\"; exit 1 ;;
      esac
    done
  "

  # O REGISTO NÃO PODE DIZER `Running` ANTES DE O CONTAINER ESTAR MONTADO.
  #
  # O check acima cobre quem passa pelo `run -d` — esse agora espera. Não cobre
  # um TERCEIRO processo (o CRI, o `serve docker-api`, uma CLI concorrente), que
  # não passa por lá: descobre o container no store e entra. Enquanto o
  # `store.save` acontecia ANTES da espera, esse terceiro lia `pid` + `Running`
  # de um processo cuja raiz ainda era a do host — medido no binário que já
  # tinha a espera, 2 de 15.
  #
  # Este não amostra uma corrida: mede a PROPRIEDADE. No instante exacto em que
  # o registo ganha `pid`, compara `/proc/<pid>/root` com `/`. Iguais = o
  # `pivot_root` ainda não aconteceu e o registo mentiu. Barato o bastante
  # (dois `stat`, sem arrancar processo nenhum) para chegar sempre à janela, ao
  # contrário de um `exec`, que leva ~50ms a arrancar e por isso quase nunca a
  # apanha — foi assim que este resíduo escapou à primeira passagem.
  cat > "$OUT/espia-registo.py" <<'ESPIA'
import json, glob, os, sys, time
root, name = os.environ["DELONIX_ROOT"], sys.argv[1]
h = os.stat("/"); hid = (h.st_dev, h.st_ino)
deadline = time.time() + 60
while time.time() < deadline:
    for f in glob.glob(os.path.join(root, "containers", "*.json")):
        try: d = json.load(open(f))
        except Exception: continue
        if d.get("name") == name and d.get("pid"):
            try: st = os.stat("/proc/%d/root" % d["pid"])
            except OSError: sys.exit(0)   # já morreu: nada a afirmar
            sys.exit(0 if (st.st_dev, st.st_ino) != hid else 1)
sys.exit(0)                               # nunca visto: não é este o check
ESPIA
  check "o registo só diz Running com o container montado" ok bash -c "
    carga=(); trap 'kill \"\${carga[@]}\" 2>/dev/null' EXIT
    for _ in \$(seq \$(nproc)); do ( while :; do :; done ) & carga+=(\$!); done
    for i in \$(seq 10); do
      n='rg-$PFX'-\$i
      python3 '$OUT/espia-registo.py' \"\$n\" & esp=\$!
      '$BIN' container run -d --name \"\$n\" '$IMG' sleep 20 >/dev/null 2>&1
      wait \$esp; rc=\$?
      '$BIN' container rm -f \"\$n\" >/dev/null 2>&1
      [ \$rc -eq 0 ] || { printf 'ciclo %s: o registo publicou o pid antes do pivot_root\n' \"\$i\"; exit 1; }
    done
  "

  check "container stop" ok "$BIN" container stop "$C"
  check "update num container parado recusa" fail "$BIN" container update "$C" --publish-add "$P2:80"
  # Parado (3) é a terceira resposta que um reconciliador precisa: existe, logo
  # não se cria — arranca-se. Antes era o mesmo 1 de «não existe» e de «rebentou».
  check "exec num container parado diz 3" 3 "$BIN" container exec "$C" /bin/true
  check "top num container parado diz 3" 3 "$BIN" container top "$C"
  check "container start" ok "$BIN" container start "$C"
  check "container rm -f" ok "$BIN" container rm -f "$C"
else
  # O `run` já foi contado como FAIL pelo `check` acima; aqui só registamos que
  # toda a bateria que dependia dele não chegou a correr.
  skip "ciclo de vida + hot reconfig do container" "o container run falhou — nada disto pôde ser exercitado"
fi

########################################
section "container: combinações de flags recusadas antes de criar"
########################################
# Antes, o `--net-bps` sem rede custom só era verificado depois do workload: em
# primeiro plano o processo corria até ao fim e o comando saía 1; com `-d` era
# aceite em silêncio. Agora é recusado antes de existir o que quer que seja.
check "run --net-bps sem rede custom recusa" 1 "$BIN" container run --name "nb-$PFX" --net none --net-bps 1mbit "$IMG" true
check "run --net-bps recusado não deixa container" 4 "$BIN" container inspect "nb-$PFX"
check "run -d --net-bps sem rede custom recusa" 1 "$BIN" container run -d --name "nbd-$PFX" --net-bps 1mbit "$IMG" sleep 5
check "run -d --net-bps recusado não deixa container" 4 "$BIN" container inspect "nbd-$PFX"
check "run --ip sem rede custom recusa" 1 "$BIN" container run -d --name "nip-$PFX" --ip 10.1.1.1 "$IMG" true

# Os reinícios que a política faz contam no RESTARTS. O supervisor só registava o
# `die` de cada execução e nunca o `start`, e o `container ls` conta starts: um
# `on-failure:2` que correu três vezes aparecia com RESTARTS 0.
check "run -d --restart on-failure:2 arranca" ok "$BIN" container run -d --net none --restart on-failure:2 --name "rc-$PFX" "$IMG" sh -c 'exit 3'
check "os reinícios da política contam no RESTARTS" ok bash -c \
  "for _ in \$(seq 1 30); do '$BIN' container ls -a -o json | python3 -c \"import json,sys; sys.exit(0 if any(c.get('names',c.get('name'))=='rc-$PFX' and c.get('restarts')==2 for c in json.load(sys.stdin)) else 1)\" && exit 0; sleep 1; done; exit 1"
"$BIN" container rm -f "rc-$PFX" >/dev/null 2>&1

# Um `stop` durante a ESPERA entre reinícios tem de ser respeitado. O supervisor
# só perguntava antes de esperar, e o `create_with` gravava a cópia antiga do
# registo por cima da flag — medido: RESTARTS 2 → 4 depois do stop. E um `start`
# na mesma janela deixava DUAS encarnações do comando, uma a sobreviver ao `rm -f`.
e2e_restarts() { "$BIN" container ls -a -o json | python3 -c "import json,sys; print(next((c.get('restarts') for c in json.load(sys.stdin) if c.get('names',c.get('name'))=='$1'),-1))"; }
e2e_status() { "$BIN" container ls -a -o json | python3 -c "import json,sys; print(next((str(c.get('status')) for c in json.load(sys.stdin) if c.get('names',c.get('name'))=='$1'),'?'))"; }
e2e_in_backoff() {  # espera até o container estar parado entre dois reinícios
  local i
  for i in $(seq 1 300); do
    [ "$(e2e_restarts "$1")" -ge "$2" ] 2>/dev/null && ! e2e_status "$1" | grep -qiE 'up|running' && return 0
    sleep 0.2
  done
  return 1
}
"$BIN" container run -d --net none --restart always --name "rb-$PFX" "$IMG" sh -c 'sleep 1; exit 1' >/dev/null 2>&1
if e2e_in_backoff "rb-$PFX" 1; then
  RB0=$(e2e_restarts "rb-$PFX")
  "$BIN" container stop -t 1 "rb-$PFX" >/dev/null 2>&1
  sleep 10
  check "um stop durante a espera entre reinícios é respeitado" ok bash -c "[ \"\$('$BIN' container ls -a -o json | python3 -c \"import json,sys; print(next((c.get('restarts') for c in json.load(sys.stdin) if c.get('names',c.get('name'))=='rb-$PFX'),-1))\")\" = '$RB0' ]"
else
  skip "um stop durante a espera entre reinícios é respeitado" "não apanhei o container entre reinícios"
fi
"$BIN" container rm -f "rb-$PFX" >/dev/null 2>&1
RS_SLEEP=$(( 3000 + RANDOM % 900 ))
"$BIN" container run -d --net none --restart always --name "rs-$PFX" "$IMG" sh -c "test -f /flag || { touch /flag; exit 1; }; exec sleep $RS_SLEEP" >/dev/null 2>&1
if e2e_in_backoff "rs-$PFX" 0; then
  "$BIN" container stop -t 1 "rs-$PFX" >/dev/null 2>&1
  "$BIN" container start "rs-$PFX" >/dev/null 2>&1
  sleep 6
  check "stop+start durante a espera não duplica a encarnação" ok bash -c "[ \"\$(pgrep -f -x 'sleep $RS_SLEEP' | wc -l)\" = 1 ]"
else
  skip "stop+start durante a espera não duplica a encarnação" "não apanhei o container entre reinícios"
fi
"$BIN" container rm -f "rs-$PFX" >/dev/null 2>&1
sleep 2
check "e o rm -f não deixa nenhuma encarnação para trás" ok bash -c "[ \"\$(pgrep -f -x 'sleep $RS_SLEEP' | wc -l)\" = 0 ]"

# Um `rename` feito na espera entre reinícios fica. O supervisor arrancava da
# cópia do registo que trazia desde o primeiro arranque, e o `create_with` grava o
# registo inteiro: o reinício seguinte repunha o nome antigo.
"$BIN" container run -d --net none --restart always --name "rn-$PFX" "$IMG" sh -c 'sleep 1; exit 1' >/dev/null 2>&1
if e2e_in_backoff "rn-$PFX" 0; then
  RN0=$(e2e_restarts "rn-$PFX")
  "$BIN" container rename "rn-$PFX" "rn2-$PFX" >/dev/null 2>&1
  check "um rename na espera entre reinícios sobrevive ao reinício" ok bash -c \
    "for _ in \$(seq 1 40); do n=\$('$BIN' container ls -a -o json | python3 -c \"import json,sys; print(next((c.get('restarts') for c in json.load(sys.stdin) if c.get('name')=='rn2-$PFX'),-1))\"); [ \"\$n\" -gt '$RN0' ] 2>/dev/null && exit 0; sleep 0.5; done; exit 1"
else
  skip "um rename na espera entre reinícios sobrevive ao reinício" "não apanhei o container entre reinícios"
fi
"$BIN" container rm -f "rn-$PFX" "rn2-$PFX" >/dev/null 2>&1

# `stop` seguido de `start` enquanto o supervisor da encarnação anterior ainda não
# registou a morte: gravava `pid = None` por cima do pid que o `start` acabara de
# gravar. O registo perdia o processo novo — `stop` já não o alcançava, cada `start`
# arrancava outro, e ele sobrevivia ao `rm -f`. A corrida depende do escalonador;
# congelar o supervisor antigo (SIGSTOP) torna-a determinística.
SS_SLEEP=$(( 5000 + RANDOM % 900 ))
"$BIN" container run -d --net none --name "ss-$PFX" "$IMG" sleep "$SS_SLEEP" >/dev/null 2>&1
sleep 1
SS_INIT=$(pgrep -f -x "sleep $SS_SLEEP" | head -1)
SS_SUP=$( [ -n "$SS_INIT" ] && ps -o ppid= -p "$SS_INIT" | tr -d ' ')
if [ -n "$SS_SUP" ] && kill -STOP "$SS_SUP" 2>/dev/null; then
  "$BIN" container stop -t 0 "ss-$PFX" >/dev/null 2>&1
  "$BIN" container start "ss-$PFX" >/dev/null 2>&1
  kill -CONT "$SS_SUP" 2>/dev/null
  sleep 2
  check "um supervisor atrasado não apaga o pid da encarnação nova" ok bash -c \
    "live=\$(pgrep -f -x 'sleep $SS_SLEEP'); [ \"\$(echo \"\$live\" | wc -w)\" = 1 ] && [ \"\$('$BIN' container inspect 'ss-$PFX' | python3 -c 'import json,sys; d=json.load(sys.stdin); d=d[0] if isinstance(d,list) else d; print(d.get(\"pid\"))')\" = \"\$live\" ]"
else
  skip "um supervisor atrasado não apaga o pid da encarnação nova" "não encontrei o supervisor do container"
fi
"$BIN" container rm -f "ss-$PFX" >/dev/null 2>&1
sleep 1
check "e o rm -f não deixa o processo para trás" ok bash -c "[ \"\$(pgrep -f -x 'sleep $SS_SLEEP' | wc -l)\" = 0 ]"
pkill -9 -f -x "sleep $SS_SLEEP" 2>/dev/null || true

# O mesmo, do lado do `stop`: um `stop` à espera (PID 1 sem handler de SIGTERM)
# enquanto o processo morre por fora e um `start` grava o processo novo. Ao
# retomar, o `stop` gravava `pid = None` por cima dele, e o processo novo
# sobrevivia ao `rm -f`. Congelar o `stop` (SIGSTOP) torna a corrida determinística.
SW_SLEEP=$(( 5000 + RANDOM % 900 ))
"$BIN" container run -d --net none --name "sw-$PFX" "$IMG" sh -c "exec sleep $SW_SLEEP" >/dev/null 2>&1
sleep 1
SW_X=$(pgrep -f -x "sleep $SW_SLEEP" | head -1)
if [ -n "$SW_X" ]; then
  "$BIN" container stop -t 30 "sw-$PFX" >/dev/null 2>&1 & SW_STOP=$!
  sleep 1
  kill -STOP "$SW_STOP" 2>/dev/null
  kill -9 "$SW_X"
  # Esperas por CONDIÇÃO, não por tempo: sob a carga da bateria, 2 s não chegavam
  # para o supervisor antigo registar a morte, o `start` via ainda `Running` e não
  # arrancava nada — o check falhava sem o motor estar errado.
  sw_pid() { "$BIN" container inspect "sw-$PFX" 2>/dev/null | python3 -c 'import json,sys; d=json.load(sys.stdin); d=d[0] if isinstance(d,list) else d; print(d.get("pid") or "")' 2>/dev/null; }
  for _ in $(seq 1 100); do [ -z "$(sw_pid)" ] && break; sleep 0.2; done
  "$BIN" container start "sw-$PFX" >/dev/null 2>&1
  for _ in $(seq 1 100); do [ -n "$(pgrep -f -x "sleep $SW_SLEEP")" ] && break; sleep 0.2; done
  kill -CONT "$SW_STOP" 2>/dev/null; wait "$SW_STOP" 2>/dev/null
  check "um stop que retoma não apaga o pid de um start feito entretanto" ok bash -c \
    "for _ in \$(seq 1 30); do live=\$(pgrep -f -x 'sleep $SW_SLEEP'); rec=\$('$BIN' container inspect 'sw-$PFX' | python3 -c 'import json,sys; d=json.load(sys.stdin); d=d[0] if isinstance(d,list) else d; print(d.get(\"pid\"))'); [ \"\$(echo \"\$live\" | wc -w)\" = 1 ] && [ \"\$rec\" = \"\$live\" ] && exit 0; sleep 0.5; done; exit 1"
else
  skip "um stop que retoma não apaga o pid de um start feito entretanto" "o container não arrancou"
fi
"$BIN" container rm -f "sw-$PFX" >/dev/null 2>&1
sleep 1
check "e o rm -f não deixa esse processo para trás" ok bash -c "[ \"\$(pgrep -f -x 'sleep $SW_SLEEP' | wc -l)\" = 0 ]"
pkill -9 -f -x "sleep $SW_SLEEP" 2>/dev/null || true

# Um perfil seccomp que permite tudo e não nomeia syscalls (ou só repete a acção
# por omissão) é válido para o Docker e o Podman, e abortava o container com 126:
# o seccompiler recusa um filtro cujas duas acções são iguais. Uma regra que
# repete o default não muda nada, e é descartada antes de construir o filtro.
SC_ALLOW="$OUT/seccomp-allow.json"
printf '{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[{"names":["read"],"action":"SCMP_ACT_ALLOW"}]}' >"$SC_ALLOW"
check "seccomp allow-all sem regras efectivas arranca" ok "$BIN" container run --rm --net none --security-opt "seccomp=$SC_ALLOW" "$IMG" true
SC_MKDIR="$OUT/seccomp-mkdir.json"
printf '{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[{"names":["mkdir","mkdirat"],"action":"SCMP_ACT_ERRNO"}]}' >"$SC_MKDIR"
check "seccomp: a regra ERRNO continua a negar" ok bash -c \
  "'$BIN' container run --rm --net none --security-opt 'seccomp=$SC_MKDIR' '$IMG' sh -c 'mkdir /tmp/x' 2>&1 | grep -q 'Operation not permitted'"

# Uma recusa DEPOIS de preparar o rootfs (aqui um `--user` que a imagem não tem)
# deixava o directório do container para trás, sem registo por onde o `rm` ou o
# `prune` o encontrassem: um por tentativa, medido. Conta-se o directório, não o
# registo — o registo nunca chegou a existir, e era isso que tornava a fuga invisível.
dirs_before=$(ls "$DELONIX_ROOT/containers" 2>/dev/null | wc -l)
check "run -u com utilizador inexistente recusa" 1 "$BIN" container run --rm --net none -u "ghost-$PFX" "$IMG" true
check "run -d -u com grupo inexistente recusa" 1 "$BIN" container run -d --net none -u "root:ghost-$PFX" "$IMG" sleep 5
check "run recusado não deixa directório em containers/" ok bash -c \
  "test \$(ls '$DELONIX_ROOT/containers' 2>/dev/null | wc -l) -eq $dirs_before"

# Um `run -d` cujo comando não arranca (binário inexistente, sem permissão de
# execução) devolvia 0 e o container aparecia `Exited (127)` a seguir — um script
# lia o 0 como sucesso. Recusa agora, com a razão do `execvp`, sem registo e sem
# directório: o overlay já montado deixa o `work/work` a 0000, que só a remoção
# mapeada leva.
dirs_before=$(ls "$DELONIX_ROOT/containers" 2>/dev/null | wc -l)
check "run -d de um binário inexistente recusa com a razão" ok bash -c \
  "out=\$('$BIN' container run -d --net none --name 'nx-$PFX' '$IMG' /nao-existe-$PFX 2>&1); rc=\$?; [ \$rc -ne 0 ] && grep -q 'did not start' <<<\"\$out\" && grep -q ENOENT <<<\"\$out\""
check "run -d de um binário inexistente não deixa registo" 4 "$BIN" container inspect "nx-$PFX"
check "nem directório em containers/" ok bash -c \
  "test \$(ls '$DELONIX_ROOT/containers' 2>/dev/null | wc -l) -eq $dirs_before"

########################################
section "container em rede custom: hot reconfig pelo ingress"
########################################
CN="cn-$PFX"
NET2="net2-$PFX"
if "$BIN" network create "$NET2" --subnet 10.252.0.0/16 >/dev/null 2>&1 && \
   "$BIN" container run -d --name "$CN" --net "$NET" "$IMG" sleep 600 >/dev/null 2>&1; then
  check "network connect a quente" ok "$BIN" network connect "$NET2" "$CN"
  check "network connect: rede extra no describe" ok bash -c "'$BIN' container describe '$CN' | grep -q '$NET2'"
  check "network connect repetido recusa" fail "$BIN" network connect "$NET2" "$CN"
  check "update: net-rate a quente" ok "$BIN" container update "$CN" --net-rate 10mbit
  check "update: taxa inválida recusa" fail "$BIN" container update "$CN" --net-rate depressa
  check "update: net-rate-clear" ok "$BIN" container update "$CN" --net-rate-clear
  check "network disconnect a quente" ok "$BIN" network disconnect "$NET2" "$CN"
  check "network disconnect de rede não ligada recusa" fail "$BIN" network disconnect "$NET2" "$CN"
  check "container update --net-connect deixou de existir (corte limpo)" 2 "$BIN" container update "$CN" --net-connect "$NET2"
  "$BIN" container rm -f "$CN" >/dev/null 2>&1
  # `--net-bps` no `run -d` era aceite e ignorado: o registo dizia a taxa e o veth
  # não tinha qdisc nenhuma (o shaping só corria depois do retorno do supervisor).
  # Prova-se no dataplane, dentro do holder — o registo é exactamente o que mentia.
  CB="cb-$PFX"
  if "$BIN" container run -d --name "$CB" --net "$NET" --net-bps 1mbit "$IMG" sleep 120 >/dev/null 2>&1; then
    PIN=$("$BIN" net netns status 2>/dev/null | grep -oE 'pin [0-9]+' | grep -oE '[0-9]+')
    check "run -d --net-bps aplica o shaping no veth" ok bash -c \
      "nsenter -t '$PIN' -U -n --preserve-credentials tc qdisc show | grep -q 'tbf .*rate 1Mbit'"
    "$BIN" container rm -f "$CB" >/dev/null 2>&1
  else
    check "run -d --net-bps numa rede custom" ok false
  fi
else
  skip "hot reconfig em rede custom" "não foi possível criar rede/container em rede custom"
fi
"$BIN" network rm "$NET2" >/dev/null 2>&1

########################################
section "stack / manifesto"
########################################
WORK="$OUT/stack-$PFX"; mkdir -p "$WORK"

# Um aviso de tradução sai UMA vez por comando. O `stack apply` traduz o mesmo
# documento para o plano e para o apply, e o aviso do emptyDir saía três vezes.
cat > "$WORK/emptydir.yaml" <<YAML
apiVersion: compute.delonix.io/v1alpha1
kind: Container
metadata: { name: ed-$PFX }
spec:
  containers:
    - name: app
      image: $IMG
      command: ["sleep", "60"]
      volumeMounts: [{ name: scratch, mountPath: /scratch }]
  volumes:
    - name: scratch
      emptyDir: {}
YAML
check "stack apply diz o aviso do emptyDir uma só vez" ok bash -c \
  "[ \"\$('$BIN' stack apply -f '$WORK/emptydir.yaml' 2>&1 | grep -c 'emptyDir without')\" = 1 ]"
"$BIN" container rm -f "ed-$PFX" >/dev/null 2>&1
cat >"$WORK/delonix-manifest.yaml" <<YAML
apiVersion: delonix.io/v1
kind: Volume
metadata:
  name: sv-$PFX
spec: {}
---
apiVersion: delonix.io/v1
kind: Network
metadata:
  name: sn-$PFX
spec:
  driver: bridge
  subnet: 10.251.0.0/16
YAML
check "stack apply" ok "$BIN" stack apply -f "$WORK/delonix-manifest.yaml"
check "stack apply idempotente" ok "$BIN" stack apply -f "$WORK/delonix-manifest.yaml"
check "stack describe" ok "$BIN" stack describe -f "$WORK/delonix-manifest.yaml"
check "volume describe do manifesto" ok "$BIN" volume describe "sv-$PFX"

# O ciclo declarativo inteiro (v0.47.0) não tinha UMA verificação aqui: o `plan`,
# o contrato de exit code que um gate de CI usa, a recusa fail-closed, e o
# `destroy`. Um manifesto INALTERADO tem de propor ZERO alterações — se propuser,
# a normalização de algum campo está a divergir dos dois lados, e o sintoma seria
# deriva eterna em todos os planos.
check "stack plan (manifesto inalterado, --detailed-exitcode = 0)" ok \
  "$BIN" stack plan -f "$WORK/delonix-manifest.yaml" --detailed-exitcode
check "stack plan --fields" ok "$BIN" stack plan -f "$WORK/delonix-manifest.yaml" --fields
check "stack validate" ok "$BIN" stack validate -f "$WORK/delonix-manifest.yaml"
check "stack apply --dry-run" ok "$BIN" stack apply -f "$WORK/delonix-manifest.yaml" --dry-run
# O `--replace` só aceita `Kind/nome`; um valor sem barra tem de ser recusado.
check "stack apply --replace mal formado recusa" fail \
  "$BIN" stack apply -f "$WORK/delonix-manifest.yaml" --replace lixo
# ---- `delonix drift` (M05): o que a MÁQUINA mudou desde o último apply ----
#
# O CICLO é o que prova alguma coisa. Cada passo isolado devolve 0 mesmo com o
# verbo partido: um `drift` que nunca reporte nada devolve 0 e imprime «sem
# deriva», que é indistinguível de um nó limpo. Por isso: aplicar, confirmar
# limpo, PROVOCAR deriva a quente, confirmar que aparece — e que o contrato de
# exit code a acompanha (a única metade que um gate de CI consome).
DRW="$WORK/drift"; mkdir -p "$DRW"
cat >"$DRW/delonix-manifest.yaml" <<YAML
apiVersion: compute.delonix.io/v1alpha1
kind: Container
metadata:
  name: dr-$PFX
spec:
  image: $IMG
  command: ["sleep", "600"]
  memory: 64M
  network: host
YAML
if "$BIN" stack apply -f "$DRW/delonix-manifest.yaml" >/dev/null 2>&1; then
  check "drift: logo a seguir ao apply não há deriva" 0 \
    "$BIN" drift -f "$DRW/delonix-manifest.yaml" --detailed-exitcode
  "$BIN" container update "dr-$PFX" --memory 128M >/dev/null 2>&1
  # A deriva TEM de nomear o campo e os dois valores. Um `--detailed-exitcode`
  # sozinho diria «algo mexeu» sem dizer o quê, e foi por uma tabela que não
  # nomeava o campo que esta bateria já passou uma vez.
  check "drift: um update a quente aparece, com campo e valores" ok bash -c \
    "'$BIN' drift -f '$DRW/delonix-manifest.yaml' | grep -q 'memory' && \
     '$BIN' drift -f '$DRW/delonix-manifest.yaml' | grep -q '64M' && \
     '$BIN' drift -f '$DRW/delonix-manifest.yaml' | grep -q '128M'"
  check "drift: --detailed-exitcode devolve 2 quando há deriva" 2 \
    "$BIN" drift -f "$DRW/delonix-manifest.yaml" --detailed-exitcode
  # SEM manifesto — o caso que dá a este verbo a sua razão de ser: um nó onde o
  # repositório não está. A deriva continua a aparecer, e os Kinds que só se
  # enumeram a partir do documento são NOMEADOS em vez de omitidos em silêncio.
  # Num directório SEM manifesto, e por isso `$DRW/vazio` e não `$DRW/..`: a
  # pasta de cima é a da secção anterior e tem lá um `delonix-manifest.yaml`,
  # que o `drift` encontra — o check passava a testar o caminho COM ficheiro
  # com o nome do caminho sem. Apanhado na primeira corrida da bateria.
  mkdir -p "$DRW/vazio"
  check "drift: sem manifesto continua a ver, e diz o que não verificou" ok bash -c \
    "cd '$DRW/vazio' && '$BIN' drift | grep -q 'memory' && '$BIN' drift | grep -q 'NOT CHECKED'"
  check "drift: --stack de outra stack não vê esta deriva" 0 \
    "$BIN" drift -f "$DRW/delonix-manifest.yaml" --stack naoexiste-$PFX --detailed-exitcode
  "$BIN" stack destroy -f "$DRW/delonix-manifest.yaml" >/dev/null 2>&1
  "$BIN" container rm -f "dr-$PFX" >/dev/null 2>&1
else
  skip "drift: o apply do container de teste não passou neste ambiente"
fi

check "stack destroy --dry-run" ok "$BIN" stack destroy -f "$WORK/delonix-manifest.yaml" --dry-run
check "stack destroy" ok "$BIN" stack destroy -f "$WORK/delonix-manifest.yaml"
# O destroy levou o que a stack possui — o `describe` a seguir tem de correr na
# mesma (parte do ficheiro, não de um registo), mas os recursos já não existem.
check "volume describe depois do destroy recusa" fail "$BIN" volume describe "sv-$PFX"
"$BIN" volume rm "sv-$PFX" >/dev/null 2>&1
"$BIN" network rm "sn-$PFX" >/dev/null 2>&1

# ---------------------------------------------------------------------------
# `system features` — níveis de maturidade derivados de EVIDÊNCIA.
#
# «Está pronto?» é a primeira pergunta que se faz a um motor 0.x, e a resposta
# vivia em prosa sem nada a prendê-la ao código. Cada linha nomeia agora a
# evidência que a sustenta, e um teste recusa uma linha sem ela.
check "system features lista" ok "$BIN" system features
check "system features -o json" ok "$BIN" system features -o json
check "…toda a capacidade traz evidência" ok bash -c \
  "'$BIN' system features -o json | python3 -c 'import json,sys; d=json.load(sys.stdin); assert all(len(f[\"evidence\"])>40 for f in d)'"
# Nada pode dizer-se `certified` sem a matriz de kernels/distros/providers que
# define esse nível — e essa matriz não existe.
check "…e nada se diz certified" ok bash -c \
  "! '$BIN' system features -o json | grep -q certified"
check "--min inválido recusa" fail "$BIN" system features --min lixo

# ---------------------------------------------------------------------------
# `policy.json` — o tecto que o NÓ põe, e que vale mesmo com a admissão do
# cluster mal configurada.
#
# Mesma razão que pôs o tecto de capabilities no CRI: tudo o que chega ao
# `cmd_run` já vem autorizado por quem chamou, e uma política que só vive na
# cadeia de admissão de um cluster corre noutra máquina que este nó não vê.
#
# Guardado por `E2E_HAVE_IMAGE`: um root isolado sem rede não tem imagem, e sem
# imagem estes checks mediriam a falta dela em vez da política.
if [[ "$E2E_HAVE_IMAGE" == "1" ]]; then
  # 1. SEM ficheiro = SEM tecto. É o caminho de upgrade: um host que nunca
  #    escreveu política comporta-se exactamente como antes.
  check "sem política, um run normal passa" ok \
    "$BIN" container run --rm --net host "$IMG" true
  # 2. Com política, recusa — e nomeia VÁRIAS razões, não só a primeira.
  cat >"$DELONIX_ROOT/policy.json" <<'JSON'
{"denyPrivileged": true, "denyHostNetwork": true, "denyLatestTag": true, "allowedRegistries": ["ghcr.io"]}
JSON
  check "com política, o run é recusado" fail \
    "$BIN" container run --rm --net host "$IMG" true
  check "…e nomeia várias razões de uma vez" ok bash -c \
    "test \$('$BIN' container run --rm --net host '$IMG' true 2>&1 | grep -c 'runtime policy') -ge 2"
  # 3. Um ficheiro que não parseia é ERRO, nunca «sem política» — um typo não
  #    pode desligar o tecto do nó em silêncio.
  echo '{ nao json' >"$DELONIX_ROOT/policy.json"
  check "uma política ilegível é erro, não ausência" fail \
    "$BIN" container run --rm --net host "$IMG" true
  # 4. E uma política que só restringe uma coisa deixa passar o resto.
  echo '{"denyPrivileged": true}' >"$DELONIX_ROOT/policy.json"
  check "política parcial: o que ela não proíbe passa" ok \
    "$BIN" container run --rm --net host "$IMG" true
  rm -f "$DELONIX_ROOT/policy.json"
else
  skip "policy.json" "sem imagem no store — ver o skip do image pull"
fi

# ---------------------------------------------------------------------------
# `scripts/sbom.py` — o SBOM que a release publica.
#
# Uma release assinada diz «isto veio de nós»; um SBOM diz «isto é feito disto»,
# e é o segundo que responde a «esta CVE afecta-me?». O gerador sai do
# `Cargo.lock`, que É a árvore resolvida — sem ferramenta de terceiros no passo
# que existe para garantir a cadeia de fornecimento.
#
# O que os checks exigem é o que um consumidor lê: SPDX válido, todo o pacote com
# versão, e o mesmo lock a dar o MESMO ficheiro (um SBOM que muda a cada corrida
# não tem nada para comparar entre duas releases).
if [[ -f "$PWD/scripts/sbom.py" ]]; then
  check "sbom.py gera" ok bash -c "python3 '$PWD/scripts/sbom.py' > '$WORK/sbom.json'"
  check "…e é SPDX 2.3 com pacotes" ok bash -c \
    "python3 -c \"import json;d=json.load(open('$WORK/sbom.json'));assert d['spdxVersion']=='SPDX-2.3';assert len(d['packages'])>50\""
  check "…todo o pacote tem nome e versão" ok bash -c \
    "python3 -c \"import json;d=json.load(open('$WORK/sbom.json'));assert all(p.get('name') and p.get('versionInfo') for p in d['packages'])\""
  # Determinístico: duas gerações do mesmo lock têm de ser byte a byte iguais.
  check "…e o mesmo lock dá o mesmo ficheiro" ok bash -c \
    "python3 '$PWD/scripts/sbom.py' > '$WORK/sbom2.json' && cmp -s '$WORK/sbom.json' '$WORK/sbom2.json'"
else
  skip "sbom.py" "não está nesta árvore"
fi

# ---------------------------------------------------------------------------
# `scripts/bench.sh` — um harness que se recusa a mentir sobre a bancada.
#
# A bateria de 2026-08-10 foi retirada por medir a contenção da máquina em vez
# das ferramentas: três motores seis vezes mais lentos ao mesmo tempo não é uma
# propriedade de nenhum. O harness passou a caracterizar a bancada ANTES e a
# recusar-se quando o load passa o limiar.
#
# Os dois checks são de CLASSE DE SAÍDA, e é isso que os torna determinísticos
# numa máquina cuja carga não controlamos: `2` = binário ausente, `3` = bancada
# recusada. Verificar o RESULTADO da medição aqui seria repetir o erro que este
# script existe para impedir — e numa máquina de CI daria ruído a cada corrida.
check "bench.sh sem binário devolve 2, e diz qual" 2 \
  bash scripts/bench.sh --bin /nao/existe/delonix
# `--max-load 0` força a recusa sem depender da carga real desta máquina. Não é
# uma flag só para teste: quem corre numa máquina DEDICADA quer ser mais estrito
# do que metade dos threads, onde um load de 1 já é alguém a fazer login.
check "bench.sh recusa uma bancada acima do limiar (3)" 3 \
  bash scripts/bench.sh --bin "$BIN" --max-load 0
# O `chaos.sh` faz o mesmo juízo, com o mesmo helper (`scripts/bancada.sh`) e a
# mesma classe de saída — e a recusa dele tem de acontecer ANTES do `setup`, que
# já levanta infra e cria rede. Um portão que só recusa depois de mexer no host
# não é um portão, é um aviso.
# Sandbox PRÓPRIO para este check, e não o `/tmp/dlx-chaos` por omissão: a
# segunda metade afirma que o directório NÃO existe, e contra o caminho
# partilhado isso dependia de nenhuma outra corrida (nem o passo de caos do
# `chaos.yml`, que corre antes deste no mesmo runner) o ter deixado de pé.
_CHDIR="$OUT/chaos-guard-$PFX"
rm -rf "$_CHDIR"
check "chaos.sh recusa uma bancada acima do limiar (3)" 3 \
  env DELONIX_CHAOS_DIR="$_CHDIR" bash scripts/chaos.sh --bin "$BIN" --max-load 0
check "…e recusa ANTES de tocar no sandbox" ok test ! -d "$_CHDIR"

# ---------------------------------------------------------------------------
# Um limite ou chega ao KERNEL ou é RECUSADO — nunca aceite e ignorado.
#
# Medido 2026-09-07, e é a razão desta secção existir: `-m 64Mi` — a grafia que
# o Kubernetes usa e que um `kind: Pod` convida — era aceite (rc=0), guardada no
# registo, mostrada pelo `inspect`, e o container corria com `memory.max = max`,
# SEM CEILING NENHUM. O caminho rootless-delegado (o normal) escrevia a string
# CRUA no ficheiro do cgroup e descartava o erro; o parser do kernel toma `64M`
# e recusa `64Mi`. O caminho ROOT, ao lado, sempre usou `write_limit`, que
# propaga. `--cpus 500m` era pior que inútil: caía num fallback de 1.0, o DOBRO
# do pedido.
#
# O teste unitário prova o parser. SÓ um check aqui prova que o binário o
# aplica — que é a metade que faltava quando o bug entrou.
# **Uma ADOPÇÃO tem de se anunciar.** O `plan` sempre disse «exists and belongs
# to no stack — will be taken over»; o `apply` não dizia nada, e a única linha
# que saía era a do handler por-Kind: `already exists, nothing to do`. Medido a
# 2026-09-10: um container criado À MÃO, um manifesto que declara esse nome, e o
# `apply` a responder «nothing to do» enquanto lhe carimbava `delonix.io/stack`
# — a partir daí o `stack destroy` (que não pergunta nada, por desenho) leva-o.
# O passo em que o recurso de outra pessoa passa a ser destruível por esta stack
# anunciava-se como «nada a fazer».
if [[ $E2E_HAVE_IMAGE -eq 1 ]]; then
  ADOPT="adopt-$PFX"
  cat > "$OUT/adopt.yaml" <<YAML
apiVersion: core.delonix.io/v1alpha1
kind: Stack
metadata: { name: st-$PFX }
spec:
  containers:
    - name: $ADOPT
      spec:
        image: $IMG
        command: ["sleep", "300"]
YAML
  "$BIN" container run -d --name "$ADOPT" "$IMG" sleep 300 >/dev/null 2>&1
  check "stack apply: uma adopção DIZ-SE (e não «nothing to do» sozinho)" ok bash -c "
    out=\$('$BIN' stack apply -f '$OUT/adopt.yaml' 2>&1)
    printf '%s\n' \"\$out\"
    printf '%s' \"\$out\" | grep -qi adopted || { echo 'o apply adoptou sem o dizer'; exit 1; }"
  check "e o carimbo de posse ficou mesmo lá" ok bash -c "
    '$BIN' container inspect '$ADOPT' | grep -q 'delonix.io/stack'"
  "$BIN" container rm -f "$ADOPT" >/dev/null 2>&1
  rm -f "$OUT/adopt.yaml"
fi

# Um `kind: Stack` com um GRUPO mal escrito (`contaienrs:`) expande para nada, e
# a mensagem que parava o comando era «<ficheiro> is empty (no YAML documents)»
# — sobre um ficheiro que o utilizador vê que não está vazio. O aviso que nomeia
# a causa real («unknown field 'contaienrs'») fica acima, e o erro apontava para
# o sítio errado.
cat > "$OUT/typo-stack.yaml" <<'YAML'
apiVersion: core.delonix.io/v1alpha1
kind: Stack
metadata: { name: typo }
spec:
  contaienrs:
    - name: c1
      spec:
        image: alpine:3.19
YAML
check "manifesto: um grupo mal escrito NÃO se chama «ficheiro vazio»" ok bash -c "
  out=\$('$BIN' stack validate -f '$OUT/typo-stack.yaml' 2>&1)
  printf '%s\n' \"\$out\"
  printf '%s' \"\$out\" | grep -q 'is empty' && { echo 'chamou vazio a um ficheiro com um documento'; exit 1; }
  printf '%s' \"\$out\" | grep -qi 'expanded to nothing' || exit 1"
rm -f "$OUT/typo-stack.yaml"

# Os grupos de um Stack saem da tabela de Kinds (ADR-0045). Quatro Kinds que o
# `stack apply` aplica (NetworkRoute, NetworkAccessRule, Service, App) não cabiam
# dentro de um Stack, e o grupo do Gateway ainda se chamava `tunnels:`. `validate`
# não escreve estado nenhum; o gate é o carregamento e a resolução de referências
# entre grupos, com a grafia antiga a misturar-se com a nova.
cat > "$OUT/groups-stack.yaml" <<'YAML'
apiVersion: core.delonix.io/v1alpha1
kind: Stack
metadata: { name: grp, namespace: e2e-grp }
spec:
  networks:
    - { name: grp-front, spec: { driver: bridge } }
    - { name: grp-back, spec: { driver: bridge } }
  networkRoutes:
    - { name: grp-r, spec: { from: grp-front, to: grp-back } }
  containers:
    - name: grp-web
      labels: { app: grp-web }
      spec: { image: "alpine:3.19", network: grp-front }
  services:
    - { name: grp-svc, spec: { selector: { matchLabels: { app: grp-web } }, port: 80 } }
  networkAccessRules:
    - { name: grp-allow, spec: { target: grp-web, direction: ingress, port: "80" } }
  gateways:
    - { name: grp-gw, spec: { provider: pinggy, localPort: 80 } }
  tunnels:
    - { name: grp-old, spec: { provider: pinggy, localPort: 81 } }
YAML
check "manifesto: um Stack aceita rotas, serviços, regras e gateways (e o antigo tunnels:)" ok "$BIN" stack validate -f "$OUT/groups-stack.yaml"
check "manifesto: uma rota para uma rede que o Stack não declara é recusada" fail bash -c "
  sed 's/to: grp-back/to: grp-nowhere/' '$OUT/groups-stack.yaml' > '$OUT/groups-stack-bad.yaml'
  '$BIN' stack validate -f '$OUT/groups-stack-bad.yaml'"
rm -f "$OUT/groups-stack.yaml" "$OUT/groups-stack-bad.yaml"

# A mesma classe, noutros dois sítios: o `Display` do `NotFound` é `no such {0}`
# e recebia frases inteiras. Medido a 2026-09-10: um `kind: Workload` com o bloco
# errado respondia «no such workload 'w1': type: vm must not carry a
# 'container:' block» — com classe **4** («não existe»), que é o que um
# reconciliador lê para decidir CRIAR, sobre um manifesto que não parseia.
cat > "$OUT/w-mismatch.yaml" <<'YAML'
apiVersion: compute.delonix.io/v1alpha1
kind: Workload
metadata: { name: wmm }
spec:
  type: vm
  container:
    image: alpine:3.19
YAML
check "workload com bloco errado é INVÁLIDO (1), não «não existe» (4)" 1 \
  "$BIN" stack validate -f "$OUT/w-mismatch.yaml"
check "e a mensagem não se lê como «no such <frase>»" ok bash -c "
  out=\$('$BIN' stack validate -f '$OUT/w-mismatch.yaml' 2>&1)
  printf '%s' \"\$out\" | grep -q 'no such workload' && { echo \"\$out\"; exit 1; }
  exit 0"
rm -f "$OUT/w-mismatch.yaml"

section "limites: o que se declara chega ao cgroup, ou é recusado"
# ACH-009, medido 2026-09-09: estes três checks chumbavam SEMPRE, e pela razão
# errada — `bash: line 1: _cg_of: command not found`. Duas causas, as duas
# fatais e as duas invisíveis num relatório que só diz FAIL:
#
#   1. O `check` invoca `bash -c`, um processo filho, e uma função de shell não
#      atravessa um `exec`. Sem `export -f` (e sem `$BIN` no ambiente) o helper
#      não existe do outro lado.
#   2. `container inspect` NÃO tem `-o` — a saída já é JSON por omissão, e o
#      `-o` nunca existiu neste verbo. Mesmo com o helper visível, ele saía 2.
#
# O que isto custou é maior do que três linhas vermelhas: a verificação de que
# um `-m` DECLARADO chega ao cgroup do kernel — a metade que faltava quando o
# bug do `64Mi` entrou, e a razão de esta secção existir — nunca correu uma
# única vez. Um FAIL constante lê-se como «defeito conhecido do produto» e é o
# esconderijo perfeito para um teste que não testa nada.
_cg_of() { # imprime o valor de um ficheiro do cgroup do container $1
  local p; p=$("$BIN" container inspect "$1" 2>/dev/null \
    | python3 -c 'import json,sys;print(json.load(sys.stdin)[0]["pid"])' 2>/dev/null) || return 1
  [ -n "$p" ] && [ "$p" != None ] || return 1
  cat "/sys/fs/cgroup$(cut -d: -f3 /proc/$p/cgroup)/$2" 2>/dev/null
}
export BIN
export -f _cg_of
_ils="$("$BIN" image ls 2>&1)"; _ils_rc=$?
if [ -n "${IMG:-}" ] && [ "$_ils_rc" -eq 0 ] && grep -q . <<<"$_ils"; then
  for spec in "64M:67108864" "64Mi:67108864" "1Gi:1073741824"; do
    _v=${spec%%:*}; _want=${spec##*:}; _n="${PFX}lim$(echo "$_v" | tr -d '.')"
    "$BIN" container rm -f "$_n" >/dev/null 2>&1 || true
    if "$BIN" container run -d --name "$_n" --net none -m "$_v" "$IMG" sleep 60 >/dev/null 2>&1; then
      # A prova é o CGROUP, não o registo: era exactamente aí que os dois
      # discordavam, com o registo a dizer 64Mi e o kernel a dizer `max`.
      check "-m $_v chega ao kernel como $_want" ok \
        bash -c "[ \"\$(_cg_of $_n memory.max)\" = $_want ]" || true
    else
      skip "-m $_v chega ao kernel" "o container não arrancou neste host"
    fi
    "$BIN" container rm -f "$_n" >/dev/null 2>&1 || true
  done
  # A outra metade: o que não se consegue ler é RECUSADO antes de criar seja o
  # que for. `64MB` e `99999999999T` estão aqui de propósito — o primeiro é a
  # grafia que toda a gente tenta, o segundo saturava para u64::MAX, que escrito
  # em `memory.max` é indistinguível de não haver limite.
  for bad in abc 64MB "99999999999T"; do
    check "-m $bad é recusado, não ignorado" fail \
      "$BIN" container run -d --name "${PFX}limbad" --net none -m "$bad" "$IMG" true
    "$BIN" container rm -f "${PFX}limbad" >/dev/null 2>&1 || true
  done
  # `500m` é millicores do Kubernetes. Recusar é o ponto: convertê-lo seria o
  # motor a adivinhar, e adivinhar dava 1 CPU inteiro.
  for bad in abc 500m; do
    check "--cpus $bad é recusado, não ignorado" fail \
      "$BIN" container run -d --name "${PFX}limcpu" --net none --cpus "$bad" "$IMG" true
    "$BIN" container rm -f "${PFX}limcpu" >/dev/null 2>&1 || true
  done
  # `--restart` é o de aresta mais afiada da classe: uma gralha (`alwyas`,
  # `on-failre`) era aceite com rc=0 e deixava `restart_policy: null` — um
  # serviço que nunca volta a subir, em silêncio, com o operador convencido de
  # que configurou um supervisor. Os outros desperdiçam recursos; este perde
  # disponibilidade.
  for bad in talvez alwyas on-failure:0 always:2; do
    check "--restart $bad é recusado, não ignorado" fail \
      "$BIN" container run -d --name "${PFX}limrs" --net none --restart "$bad" "$IMG" true
    "$BIN" container rm -f "${PFX}limrs" >/dev/null 2>&1 || true
  done
  # Os pesos documentavam `1–10000` e não verificavam nada: `0`, `99999` e `abc`
  # caíam todos no default do kernel (100), com rc=0. Uma gama no `--help` que o
  # código não impõe é uma promessa, não um facto.
  for bad in 0 10001 abc; do
    check "--cpu-weight $bad é recusado, não ignorado" fail \
      "$BIN" container run -d --name "${PFX}limw" --net none --cpu-weight "$bad" "$IMG" true
    "$BIN" container rm -f "${PFX}limw" >/dev/null 2>&1 || true
  done
  # E o que passa tem de CHEGAR ao registo: aceitar a política e não a guardar é
  # o mesmo defeito com outra roupa.
  "$BIN" container rm -f "${PFX}limrs" >/dev/null 2>&1 || true
  if "$BIN" container run -d --name "${PFX}limrs" --net none --restart on-failure:3 "$IMG" sleep 30 >/dev/null 2>&1; then
    check "--restart on-failure:3 fica no registo" ok \
      bash -c "'$BIN' container inspect ${PFX}limrs | grep -q 'on-failure:3'" || true
  else
    skip "--restart on-failure:3 fica no registo" "o container não arrancou neste host"
  fi
  "$BIN" container rm -f "${PFX}limrs" >/dev/null 2>&1 || true
else
  skip "limites: chegam ao cgroup" "sem imagem no store (precisa de rede para o pull) (image ls rc=$_ils_rc: $(head -c 300 <<<"$_ils" | tr '\n' ' '))"
fi

########################################
# A secção acima prova que o número DECLARADO chega ao ficheiro do cgroup. Não
# prova que o kernel o IMPÕE — e são coisas diferentes: `memory.max` escrito com
# o valor certo num cgroup cujo controlador não está delegado lê-se igualzinho e
# não limita nada. É a diferença entre «o formulário foi preenchido» e «a porta
# está trancada», e era metade que faltava: das 689 verificações desta bateria,
# ZERO exercitavam um workload a TENTAR passar do que lhe foi permitido.
#
# O que se mede aqui é o comportamento sob tentativa de abuso, sempre com prova
# de DOIS lados — que o tecto corta quem o excede E que deixa passar quem cabe.
# Um teste só do lado de cima passa num motor que recusa tudo.
#
# Medido a 2026-09-09 neste host (32 cores, 30,5 GiB, rootless com `cpu memory
# pids` delegados): `-m 64M` deixa alocar 16 MiB e mata aos 256 MiB (rc=137);
# 1500 forks param nos 512 de `pids.max` com `can't fork`; e 4 ciclos ocupados
# sob `--cpus 0.5` consomem 0,500 cores com `nr_throttled=217`.
#
# O que ISTO NÃO PROVA, e está declarado onde se paga: o tecto de I/O de disco.
# O `user@<uid>.service` do systemd nunca delega o controlador `io` a um
# utilizador rootless, por isso `io.max` não existe na base e um container PODE
# saturar o disco. O motor di-lo em voz alta (aviso no `run`, `DLX-RES-002` no
# `system doctor`) — mas dizer não é impor, e por isso não há aqui um check a
# fingir que há tecto.
########################################
section "limites: o que se declara é IMPOSTO, não só escrito"

_leaf_of() { # imprime o caminho do cgroup da leaf do container $1
  local p; p=$("$BIN" container inspect "$1" 2>/dev/null \
    | python3 -c 'import json,sys;print(json.load(sys.stdin)[0]["pid"])' 2>/dev/null) || return 1
  [ -n "$p" ] && [ "$p" != None ] || return 1
  echo "/sys/fs/cgroup$(cut -d: -f3 "/proc/$p/cgroup" 2>/dev/null)"
}
export -f _leaf_of
export IMG PFX

_ils="$("$BIN" image ls 2>&1)"; _ils_rc=$?
if [ -n "${IMG:-}" ] && [ "$_ils_rc" -eq 0 ] && grep -q . <<<"$_ils"; then

  # --- MEMÓRIA: o tecto corta, e corta no sítio certo ------------------------
  #
  # `dd` aloca um buffer do tamanho de `bs`, o que faz dele um alocador LIMITADO
  # e determinístico — ao contrário de um `tail /dev/zero`, que cresce até
  # alguém o matar e por isso só sabe responder «morreu», nunca «morreu no
  # sítio certo». Com 16 MiB dentro de um tecto de 64 MiB tem de passar; com
  # 256 MiB tem de ser morto pelo kernel.
  check "-m 64M: 16 MiB cabem (o tecto não estrangula quem respeita)" ok \
    bash -c 'timeout 60 "$BIN" container run --rm --net none -m 64M "$IMG" \
             dd if=/dev/zero of=/dev/null bs=16M count=1'
  check "-m 64M: 256 MiB são MORTOS pelo kernel (o tecto é imposto)" fail \
    bash -c 'timeout 60 "$BIN" container run --rm --net none -m 64M "$IMG" \
             dd if=/dev/zero of=/dev/null bs=256M count=1'
  # E o mesmo pedido tem de passar sem tecto declarado — senão o check acima
  # estaria a medir um `dd` que falha por outra razão qualquer.
  check "…e os mesmos 256 MiB passam sem -m (era mesmo o tecto)" ok \
    bash -c 'timeout 60 "$BIN" container run --rm --net none "$IMG" \
             dd if=/dev/zero of=/dev/null bs=256M count=1'


  # --- ACH-016 (CORRIGIDO): os filhos do workload também levam com o tecto ---
  #
  # O tecto de memória chegou a aplicar-se ao PID 1 do container e a MAIS
  # NINGUÉM: tudo o que o workload forkasse corria fora da leaf, sem tecto
  # nenhum — e quase todo o workload real forka (o nginx lança workers, o postgres lança backends, um
  # entrypoint em shell lança o que lhe mandarem).
  #
  # Medido a 2026-09-09 neste host, com `-m 64M`:
  #   alocar 256 MiB no PRÓPRIO PID 1  -> rc=137, morto pelo kernel  (correcto)
  #   alocar 256 MiB num FILHO forkado -> rc=0, 3/3                  (escapa)
  #   alocar 2 GiB   num FILHO forkado -> rc=0, ou seja 32x o tecto  (escapa)
  # E os filhos aparecem no `cgroup.procs` do cgroup de QUEM INVOCOU o
  # `delonix`, não na leaf `dlx-<id>` — fora do `dlx-containers`, portanto fora
  # também do tecto agregado de 85% que protege o host.
  #
  # CAUSA, lida no código e não deduzida (`crates/adapters/delonix-linux/src/lib.rs`):
  # o `setup_cgroup` corria DEPOIS do byte "GO" que liberta o filho para executar
  # o entrypoint, e a migração de cgroup v2 move UM processo, nunca a sua
  # descendência. O comentário no sítio raciocina sobre a janela — «every
  # millisecond it is not in one is a millisecond it runs uncapped» — e o que lhe
  # escapa é que a janela dura milissegundos mas a consequência é PERMANENTE:
  # quem for forkado lá dentro fica fora da leaf para sempre. O padrão da
  # correcção já existe a três funções de distância, e está escrito no próprio
  # ficheiro para a REDE: «NETWORK BEFORE THE GO (critical order): the child is
  # still BLOCKED waiting ... so the network is ready BEFORE the entrypoint
  # runs». O cgroup passou a ter o mesmo tratamento — e este check é o que
  # impede a regressão: medido depois da correcção, 5/5 filhos mortos, com os
  # 3 processos na leaf e o CPU estrangulado nos 0,50 cores pedidos.
  #
  # TRÊS TENTATIVAS, e não uma, de propósito: era uma corrida, e o primeiro
  # container de um host chegava a ganhá-la (medido: 1 correcto em 6). Uma só
  # tentativa deixaria uma regressão passar de vez em quando — que é a quarta
  # forma de não testar nada descrita no cabeçalho desta bateria.
  check "-m 64M: um FILHO forkado também é morto (não só o PID 1)" ok bash -c '
    for i in 1 2 3; do
      timeout 60 "$BIN" container run --rm --net none -m 64M "$IMG" \
        sh -c "dd if=/dev/zero of=/dev/null bs=256M count=1 & wait" >/dev/null 2>&1
      if [ $? -eq 0 ]; then
        echo "tentativa $i: o filho alocou 256 MiB com um tecto de 64 MiB — o tecto não o cobre"
        exit 1
      fi
    done
  '

  # --- O QUE UM CONTAINER LEVA SEM PEDIR NADA -------------------------------
  #
  # Um container sem uma única flag não pode ficar sem tecto: era exactamente
  # isso que fazia «sem limite» querer dizer «o host inteiro». Três propriedades
  # numa só corrida, porque as três vivem na mesma leaf.
  _n="${PFX}enfdef"
  "$BIN" container rm -f "$_n" >/dev/null 2>&1 || true
  if "$BIN" container run -d --name "$_n" --net none "$IMG" sleep 60 >/dev/null 2>&1; then
    check "sem flags: a leaf TEM tecto de memória (não 'max')" ok \
      bash -c '[ "$(cat "$(_leaf_of '"$_n"')/memory.max")" != max ]'
    # Sem isto o tecto de memória é contornável: as páginas saem para swap e o
    # container passa do que lhe foi permitido sem nunca tocar em `memory.max`.
    check "sem flags: memory.swap.max=0 (não se escapa ao tecto pelo swap)" ok \
      bash -c '[ "$(cat "$(_leaf_of '"$_n"')/memory.swap.max")" = 0 ]'
    check "sem flags: pids.max=512 (anti fork-bomb por omissão)" ok \
      bash -c '[ "$(cat "$(_leaf_of '"$_n"')/pids.max")" = 512 ]'
  else
    skip "sem flags: os tectos por omissão" "o container não arrancou neste host"
  fi
  "$BIN" container rm -f "$_n" >/dev/null 2>&1 || true

  # --- PIDs: a fork-bomb pára dentro, e o host não dá por ela ---------------
  #
  # 1500 forks contra um tecto de 512. A prova é de dois lados: o container tem
  # de FALHAR (o kernel recusou-lhe processos) e a contagem de processos do HOST
  # tem de ficar na mesma — um tecto que só matasse o container depois de ele
  # encher a tabela de processos do host não serviria de nada.
  check "fork-bomb: pára nos 512 e o host não perde processos" ok bash -c '
    antes=$(ps -e --no-headers | wc -l)
    "$BIN" container rm -f '"${PFX}"'enffork >/dev/null 2>&1
    timeout 90 "$BIN" container run --name '"${PFX}"'enffork --net none "$IMG" \
      sh -c "i=0; while [ \$i -lt 1500 ]; do sleep 60 & i=\$((i+1)); done" >/dev/null 2>&1
    rc=$?
    "$BIN" container rm -f '"${PFX}"'enffork >/dev/null 2>&1
    depois=$(ps -e --no-headers | wc -l)
    [ $rc -ne 0 ] || { echo "1500 forks passaram com pids.max=512 — o tecto não foi imposto"; exit 1; }
    d=$(( depois - antes ))
    [ "$d" -lt 200 ] || { echo "o host ganhou $d processos — a fork-bomb escapou ao cgroup"; exit 1; }
  '

  # --- CPU: o quota estrangula mesmo ---------------------------------------
  #
  # Quatro ciclos ocupados sob `--cpus 0.5` consumiriam 4 cores sem quota. A
  # medição é o delta de `cpu.stat/usage_usec` sobre uma janela de 5s, comparado
  # com o que foi pedido. A tolerância é larga de propósito (até 0,75 core para
  # um pedido de 0,5): num host carregado a contabilidade do kernel oscila, e um
  # limiar apertado dava um check a piscar — que é a quarta forma de não testar
  # nada que o cabeçalho desta bateria descreve. Mesmo 0,75 está muito abaixo
  # dos 4 cores que passariam sem quota nenhuma, que é o que isto tem de apanhar.
  _n="${PFX}enfcpu"
  "$BIN" container rm -f "$_n" >/dev/null 2>&1 || true
  if "$BIN" container run -d --name "$_n" --net none --cpus 0.5 "$IMG" \
       sh -c 'i=0; while [ $i -lt 4 ]; do (while :; do :; done) & i=$((i+1)); done; sleep 120' >/dev/null 2>&1 \
     && sleep 3 && [ -r "$(_leaf_of "$_n" 2>/dev/null)/cpu.stat" ]; then
    check "--cpus 0.5: 4 ciclos ocupados ficam por ~0.5 core (não 4)" ok bash -c '
      cg=$(_leaf_of '"$_n"') || exit 1
      # A PRÉ-CONDIÇÃO PRIMEIRO. Um cgroup onde só está o PID 1 lê 0 cores, e
      # 0 <= 0.75 daria PASS a um motor que não conta os filhos — foi
      # exactamente assim que este check passou vazio enquanto o ACH-016
      # reproduzia. Um check tem de falhar quando não consegue medir.
      n=$(wc -l < "$cg/cgroup.procs")
      [ "$n" -ge 3 ] || { echo "só $n processo(s) na leaf: os ciclos ocupados não estão contabilizados (ACH-016?)"; exit 1; }
      u1=$(awk "/usage_usec/{print \$2}" "$cg/cpu.stat"); sleep 5
      u2=$(awk "/usage_usec/{print \$2}" "$cg/cpu.stat")
      python3 -c "
import sys
cores = ($u2 - $u1) / 5e6
print(\"cores consumidos = %.3f (pedido: 0.5)\" % cores)
if cores < 0.05:
    print(\"nada foi consumido — a medição não vale\"); sys.exit(1)
sys.exit(0 if cores <= 0.75 else 1)"
    '
  else
    skip "--cpus 0.5 estrangula mesmo" "este cgroup não delega o controlador cpu — a quota não é imponível aqui"
  fi
  "$BIN" container rm -f "$_n" >/dev/null 2>&1 || true

  # --- O TECTO AGREGADO: a soma de todos também tem limite ------------------
  #
  # Cada container pode estar dentro do seu tecto e a SOMA matar o host na mesma.
  # É a razão de existir do orçamento no cgroup pai (85% do host por omissão);
  # em rootless ele não existiu durante versões, e «sem limite» era mesmo sem
  # limite. Verifica-se onde os workloads deste utilizador realmente vivem: o
  # PAI da leaf, seja ele a `delonix.slice` (root) ou a base delegada (rootless).
  _n="${PFX}enfagg"
  "$BIN" container rm -f "$_n" >/dev/null 2>&1 || true
  if "$BIN" container run -d --name "$_n" --net none "$IMG" sleep 60 >/dev/null 2>&1; then
    check "o cgroup PAI tem tecto agregado de memória (a soma não mata o host)" ok \
      bash -c '[ "$(cat "$(dirname "$(_leaf_of '"$_n"')")/memory.max")" != max ]'
    check "…e tecto agregado de pids" ok \
      bash -c '[ "$(cat "$(dirname "$(_leaf_of '"$_n"')")/pids.max")" != max ]'
  else
    skip "tecto agregado" "o container não arrancou neste host"
  fi
  "$BIN" container rm -f "$_n" >/dev/null 2>&1 || true

  # --- FUGA: 20 ciclos não podem deixar rasto -------------------------------
  #
  # Um motor que limita bem cada container e deixa restos a cada corrida acaba
  # no mesmo sítio, só mais devagar. Mede-se o que este repo já viu vazar:
  # directórios de container, descritores de ficheiro do processo, e cgroups
  # sem processos nenhuns. O `dlx-*` conta-se por NOME de leaf viva: um cgroup
  # sem processos que sobreviva aos 20 ciclos é rasto, não trabalho em curso.
  check "20 ciclos de run/rm não deixam cgroups nem directórios para trás" ok bash -c '
    dirs0=$(ls "$DELONIX_ROOT/containers" 2>/dev/null | wc -l)
    fds0=$(ls /proc/self/fd | wc -l)
    for i in $(seq 20); do "$BIN" container run --rm --net none "$IMG" true >/dev/null 2>&1; done
    dirs1=$(ls "$DELONIX_ROOT/containers" 2>/dev/null | wc -l)
    fds1=$(ls /proc/self/fd | wc -l)
    [ "$dirs1" -le "$dirs0" ] || { echo "ficaram $((dirs1-dirs0)) registos de container por limpar"; exit 1; }
    [ "$fds1" -le "$((fds0+2))" ] || { echo "vazaram $((fds1-fds0)) descritores"; exit 1; }
  '
else
  skip "limites: são impostos, não só escritos" "sem imagem no store (precisa de rede para o pull) (image ls rc=$_ils_rc: $(head -c 300 <<<"$_ils" | tr '\n' ' '))"
fi

# ---------------------------------------------------------------------------
# `system doctor` — o host mente em silêncio, e alguém tem de perguntar.
#
# Vários pré-requisitos falham SEM DIZER: sem `br_netfilter` o isolamento de
# namespace é instalado, os sets são preenchidos, todo o comando reporta
# sucesso — e a fronteira não existe (medido 2026-08-12 numa VM limpa: teamA
# alcançou teamB). Sem delegação de cgroup, `-m`/`--cpus` são aceites e
# ignorados. O `doctor` não recusa nada e não muda nada: só pergunta.
check "system doctor corre" ok "$BIN" system doctor
check "…e nomeia o pré-requisito que falha em silêncio" ok \
  bash -c "'$BIN' system doctor | grep -q br_netfilter"
# Um diagnóstico não é um portão: sem `--strict` devolve 0 mesmo num host
# imperfeito, senão ninguém o corre duas vezes.
check "system doctor sem --strict não falha" ok \
  bash -c "PATH=/nonexistent '$BIN' system doctor >/dev/null 2>&1"
# Com `--strict` é que vira portão — e tem de DETECTAR mesmo. Um doctor que só
# sabe dizer «está tudo bem» não vale nada, por isso o teste é contra um host
# onde as ferramentas não existem.
check "system doctor --strict detecta um host sem as ferramentas" fail \
  bash -c "PATH=/nonexistent '$BIN' system doctor --strict"
# Três promessas do dataplane que só falham SOB CARGA e nunca ao correr o
# doctor em repouso: conntrack cheio dropa pacotes novos em silêncio, a
# tabela ARP cheia deixa vizinhos inalcançáveis, e uma gama de portas
# efémeras estreita esgota sob muitas ligações SNAT simultâneas. O `doctor`
# não pode provar a carga — só que a MEDIÇÃO aparece, com o número real ao
# lado (não um sim/não sem contexto).
check "…mede a tabela conntrack, não só se existe" ok \
  bash -c "'$BIN' system doctor | grep -q 'conntrack table'"
check "…mede a tabela de vizinhos ARP" ok \
  bash -c "'$BIN' system doctor | grep -q 'ARP/neighbour table'"
check "…mede a largura da gama de portas efémeras" ok \
  bash -c "'$BIN' system doctor | grep -q 'ephemeral port range'"

# ---------------------------------------------------------------------------
# A matriz de compatibilidade da Docker Engine API tem de dizer TRÊS estados.
#
# A regra da casa é que «Docker-compatible» nunca viaja sem número, data e
# versão, e que a matriz mostra servido / recusado com razão / em falta — nunca
# dois. Tinha dois: quem lia não distinguia «não implementado» de «ninguém
# pensou nisto», e `POST /images/create` (o pull que quase toda a ferramenta faz
# primeiro) não aparecia em lista nenhuma.
check "serve docker-api --matrix corre" ok "$BIN" serve docker-api --matrix
check "…e traz o número e a versão no cabeçalho" ok \
  bash -c "'$BIN' serve docker-api --matrix | head -1 | grep -qE 'delonix [0-9]+\\.[0-9]+.*served.*refused'"
check "…e mostra o terceiro estado (o que as ferramentas usam)" ok \
  bash -c "'$BIN' serve docker-api --matrix | grep -q 'SEEN IN'"
# A rota que decide se o Testcontainers arranca. Estar recusada é uma resposta;
# não estar em lado nenhum não é.
#
# **O check exigia `refused|not written` e passou a FALHAR quando a rota foi
# SERVIDA** (#460, 2026-09-22): ficou a pedir a prova de que ela continuava por
# fazer. É a armadilha do «um teste pode codificar o bug», aqui virada do avesso
# — codificou a ausência da funcionalidade. O que a linha quer dizer é que a
# rota está CLASSIFICADA, e é isso que se exige agora: aparece na matriz, seja
# em que estado for.
check "…e o pull aparece classificado, não em silêncio" ok \
  bash -c "'$BIN' serve docker-api --matrix | grep -q '/images/create'"

# ---------------------------------------------------------------------------
# `stack history` (ADR-0019) — e a propriedade que o desenho inteiro promete.
#
# Uma revisão é um REGISTO do que foi pedido, nunca uma fonte de verdade sobre o
# que existe. É essa distinção que separa isto de um `terraform.tfstate`, e ela
# só vale se for verificável — daí os últimos checks: **apagar `<root>/stacks/`
# e todo o resto continua a funcionar**. Sem esse gate, a promessa é uma frase.
#
# **Directório próprio, e a primeira versão deste bloco não o tinha.** Sem um
# `kind: Stack`, a identidade de uma stack é o DIRECTÓRIO do manifesto — e este
# `$WORK` já teve outros applies antes deste ponto, por isso `--show 1` devolvia
# a revisão de OUTRO ficheiro e o check falhava com o motor certo. Vale a pena
# reter: manifestos vizinhos partilham histórico, tal como já partilham posse.
#
# **Assimetria encontrada aqui, e não corrigida neste bloco**: `plan`, `destroy`
# e `prune` aceitam `--name` e o `apply` NÃO. Logo pode planear-se e destruir-se
# sob um nome que nenhum apply alguma vez usou. Está registado; mexer no `apply`
# é a superfície mais sensível do grupo e não pertence a um check de E2E.
HWORK="$WORK/h-$PFX"
mkdir -p "$HWORK"
cat >"$HWORK/hist.yaml" <<YAML
apiVersion: delonix.io/v1
kind: Volume
metadata:
  name: hv-$PFX
spec: {}
YAML
check "stack apply num directório próprio" ok "$BIN" stack apply -f "$HWORK/hist.yaml"
# Mede a GRAVAÇÃO e não o apply: um check pelo rc do `apply` passaria com o
# registo por escrever, que é exactamente o defeito que este bloco existe para
# apanhar.
check "o apply gravou mesmo uma revisão" ok \
  bash -c "test \$('$BIN' stack history -f '$HWORK/hist.yaml' -o json | python3 -c 'import json,sys; print(len(json.load(sys.stdin)))') -ge 1"
check "stack history lista" ok "$BIN" stack history -f "$HWORK/hist.yaml"
# O manifesto renderizado da revisão 1 tem de nomear o recurso que ela aplicou —
# um `--show` que devolva 0 com o ficheiro errado passaria um check por rc.
check "stack history --show devolve o manifesto aplicado" ok \
  bash -c "'$BIN' stack history -f '$HWORK/hist.yaml' --show 1 | grep -q 'hv-$PFX'"
# Uma revisão que não existe é «não existe» (classe 4), não um 1 genérico.
check "stack history --show inexistente devolve 4" 4 \
  "$BIN" stack history -f "$HWORK/hist.yaml" --show 999
# Um apply que NÃO PEDE NADA não gasta uma revisão. A retenção é 20 e é o
# escritor que poda, por isso re-aplicar um manifesto inalterado empurrava para
# fora a revisão que mudou alguma coisa — medido antes da correcção: quatro
# applies do mesmo ficheiro davam quatro revisões, com `plan
# --detailed-exitcode` a responder 0 o tempo todo. Um alvo GitOps a reconciliar
# de minuto a minuto (ADR-0021) apagava o próprio histórico em vinte minutos.
#
# O check compara a contagem ANTES e DEPOIS de três applies. Um check pelo rc do
# `apply` passaria com o defeito inteiro lá dentro, e um que só olhasse para o
# fim não distinguiria «não gravou» de «gravou e podou».
check "três applies sem alterações não gastam revisões" ok \
  bash -c "n() { '$BIN' stack history -f '$HWORK/hist.yaml' -o json | python3 -c 'import json,sys; print(len(json.load(sys.stdin)))'; }; \
    before=\$(n); for i in 1 2 3; do '$BIN' stack apply -f '$HWORK/hist.yaml' >/dev/null 2>&1; done; \
    after=\$(n); test \"\$before\" = \"\$after\""
# ...e o plano confirma que era mesmo um no-op: sem isto, o check acima também
# passaria num motor que simplesmente parou de gravar revisões de todo.
check "e o plano confirma que não havia nada a mudar" ok \
  "$BIN" stack plan -f "$HWORK/hist.yaml" --detailed-exitcode
# `stack apply --name` — o nome que se destrói tem de poder ser CRIADO.
#
# Medido a 2026-08-25: `plan`, `destroy`, `prune`, `history` e `rollback`
# aceitavam `--name`; o `apply` não. Ou seja, dava para planear e destruir sob um
# nome que nenhum apply alguma vez usara — o `apply` derivava sempre a posse do
# directório do manifesto. O ciclo abaixo é o que a assimetria tornava
# impossível, e cada passo isolado devolve 0 mesmo com ela presente.
#
# **Recurso PRÓPRIO, e a primeira versão não o tinha.** Reutilizava o volume do
# bloco acima, que já pertence a outra stack — e o motor RECUSA, correctamente,
# dois donos para o mesmo recurso (`Conflict`). O teste falhava com o código
# certo. Vale a pena reter: um manifesto novo sob um nome novo precisa de
# recursos novos, senão o que se mede é a regra de posse e não o `--name`.
OWORK="$WORK/own-$PFX"
mkdir -p "$OWORK"
cat >"$OWORK/m.yaml" <<YAML
apiVersion: delonix.io/v1
kind: Volume
metadata:
  name: ov-$PFX
spec: {}
YAML
check "stack apply --name carimba a posse com esse nome" ok \
  "$BIN" stack apply -f "$OWORK/m.yaml" --name "own-$PFX"
check "…e o history desse nome vê a revisão" ok \
  bash -c "test \$('$BIN' stack history --name 'own-$PFX' -o json | python3 -c 'import json,sys; print(len(json.load(sys.stdin)))') -ge 1"
# A prova de que o carimbo é MESMO aquele nome: sem `--name` o apply teria
# carimbado o directório, e um destroy sob `own-…` não encontraria nada.
check "…e o destroy desse nome leva o que ele criou" ok \
  "$BIN" stack destroy -f "$OWORK/m.yaml" --name "own-$PFX"
check "…e já não está" fail "$BIN" volume inspect "ov-$PFX"

# `stack rollback` — o CICLO, e não os comandos um a um.
#
# Cada passo isolado devolve 0 com e sem a funcionalidade a funcionar; o que
# distingue é o ESTADO no fim. Por isso: aplica-se A, aplica-se B, volta-se a A,
# e verifica-se o que voltou (a quota) e o que NÃO voltou (o recurso criado em
# B, que só sai com `--prune`) — que é a promessa que o comando faz e a que ele
# recusa fazer.
cat >"$HWORK/hist.yaml" <<YAML
apiVersion: delonix.io/v1
kind: Volume
metadata:
  name: hv-$PFX
spec:
  quota: 9G
---
apiVersion: delonix.io/v1
kind: Volume
metadata:
  name: hv2-$PFX
spec: {}
YAML
check "um segundo apply, com um recurso novo e um campo mudado" ok \
  "$BIN" stack apply -f "$HWORK/hist.yaml"
check "rollback --dry-run não muda nada" ok \
  "$BIN" stack rollback --to 1 -f "$HWORK/hist.yaml" --dry-run
check "…e o recurso da 2.ª revisão continua lá depois do dry-run" ok \
  "$BIN" volume inspect "hv2-$PFX"
# Uma revisão que não existe é «não existe» (4); uma revisão FALHADA é um
# argumento inválido (1) — está no registo para ser LIDA, não para ser repetida.
check "rollback para uma revisão inexistente devolve 4" 4 \
  "$BIN" stack rollback --to 999 -f "$HWORK/hist.yaml"
# Uma revisão falhada de verdade, e barata: um `kind: Vm` com um disco que não
# existe morre DEPOIS da camada Volume, é local e é instantâneo — nada de rede,
# que num gate seria lento e dependeria do host ter saída.
cat >"$HWORK/mau.yaml" <<YAML
apiVersion: delonix.io/v1
kind: Volume
metadata:
  name: hv-$PFX
spec: {}
---
apiVersion: delonix.io/v1
kind: Vm
metadata:
  name: hvm-$PFX
spec:
  disk: /nao/existe/de/todo.qcow2
YAML
check "um apply falhado é gravado na história" fail "$BIN" stack apply -f "$HWORK/mau.yaml"
check "…e aparece marcado como falhado" ok \
  bash -c "'$BIN' stack history -f '$HWORK/hist.yaml' | grep -q 'failed'"
# A revisão falhada é a última; o rollback para ela tem de ser RECUSADO.
#
# **Verifica a MENSAGEM e não o código de saída, e a primeira versão fazia o
# contrário.** Medido: com a recusa desactivada o check continuava a passar,
# porque replicar um manifesto que não aplica também falha — o rc não distingue
# «recusado à cabeça» de «tentou e rebentou a meio», e a diferença entre os dois
# é a funcionalidade inteira. A frase que só a recusa produz é «FAILED apply».
check "rollback para uma revisão FALHADA é recusado à cabeça" ok bash -c \
  "N=\$('$BIN' stack history -f '$HWORK/hist.yaml' -o json | python3 -c 'import json,sys; print([r[\"number\"] for r in json.load(sys.stdin) if not r[\"ok\"]][-1])'); '$BIN' stack rollback --to \$N -f '$HWORK/hist.yaml' 2>&1 | grep -q 'FAILED apply'"
check "rollback --to 1 corre" ok "$BIN" stack rollback --to 1 -f "$HWORK/hist.yaml"
# O que VOLTOU: o campo que a revisão 1 declarava. Sem `quota:`, o volume da
# revisão 1 não tem cap, por isso o `inspect` não pode mostrar os 9G de B.
check "o campo mudado voltou ao valor da revisão 1" ok \
  bash -c "! '$BIN' volume inspect 'hv-$PFX' | grep -qi '9663676416'"
# O que NÃO voltou, e é dito em vez de escondido: um rollback não apaga sozinho.
check "o recurso criado depois SOBREVIVE a um rollback sem --prune" ok \
  "$BIN" volume inspect "hv2-$PFX"
check "…e o rollback avisa que é preciso --prune para o levar" ok \
  bash -c "'$BIN' stack rollback --to 1 -f '$HWORK/hist.yaml' --dry-run 2>&1 | grep -q -- '--prune'"
check "rollback --prune leva-o" ok \
  "$BIN" stack rollback --to 1 -f "$HWORK/hist.yaml" --prune
check "…e agora já não está" fail "$BIN" volume inspect "hv2-$PFX"
# Um rollback É um apply: ganha revisão própria, e a história diz de qual veio.
check "a história marca o rollback e diz que revisão replicou" ok \
  bash -c "'$BIN' stack history -f '$HWORK/hist.yaml' | grep -q 'rollback of 1'"

# A propriedade central do ADR-0019: o registo não é fonte de verdade nenhuma.
rm -rf "$DELONIX_ROOT/stacks"
check "sem stacks/: o plan continua a funcionar" ok "$BIN" stack plan -f "$HWORK/hist.yaml"
check "sem stacks/: o destroy continua a funcionar" ok "$BIN" stack destroy -f "$HWORK/hist.yaml"
check "sem stacks/: o history diz que não há, sem falhar" ok \
  "$BIN" stack history -f "$HWORK/hist.yaml"
"$BIN" volume rm "hv-$PFX" >/dev/null 2>&1

# `stack wait` não tinha UM check — e era o balde dos comandos nunca executados a
# pagar-se outra vez. O `wait` decidia prontidão com `present == "yes"`, e os
# Kinds declarativos devolvem `-`: QUALQUER manifesto com um deles esgotava o
# `--timeout` inteiro e saía com erro sobre uma stack inteiramente a correr.
# Estes documentos não criam recurso nenhum, por isso o gate é instantâneo.
#
# **O `NetworkRoute` NÃO pertence aqui, e estava.** Ele foi declarativo, e
# deixou de o ser quando ganhou registo próprio (`infra::RouteDef`) — o
# `presence` responde-lhe `yes`/`no` como a qualquer recurso com estado. Com
# duas redes que este manifesto não cria, a rota está mesmo AUSENTE, o `wait`
# espera-a até ao fim do `--timeout` e sai ≠0: o check falhava por o motor estar
# certo. Um teste que fixa o comportamento errado é a armadilha que este repo já
# pagou (ver AGENTS.md, «um teste pode codificar o bug»), e o sintoma aqui era
# indistinguível de uma regressão no `wait`.
cat >"$WORK/declarativos.yaml" <<YAML
apiVersion: delonix.io/v1
kind: FirewallPolicy
metadata:
  name: wpol-$PFX
spec:
  direction: ingress
  target: walvo-$PFX
  rules:
    - port: 80
      action: allow
---
apiVersion: delonix.io/v1
kind: HTTPRoute
metadata:
  name: wrota-$PFX
spec:
  rules:
    - host: wait-$PFX.test
      backends:
        - service: walvo-$PFX
          port: 80
YAML
# O `--timeout 5` é o que distingue: antes da correcção esperava-o por inteiro e
# saía ≠0; agora responde de imediato.
check "stack wait com Kinds declarativos não espera pelo timeout" ok \
  "$BIN" stack wait -f "$WORK/declarativos.yaml" --timeout 5
# A marca `-` deixou de ser confundida com «ausente», mas `no` TEM de continuar a
# esperar — senão a correcção tornou tudo pronto, que é o mesmo defeito ao contrário.
cat >"$WORK/ausente.yaml" <<YAML
apiVersion: delonix.io/v1
kind: Container
metadata:
  name: wausente-$PFX
spec:
  image: alpine:latest
YAML
check "stack wait continua a esperar por um recurso ausente" fail \
  "$BIN" stack wait -f "$WORK/ausente.yaml" --timeout 3
# O `NetworkRoute` está em KINDS e o `apply` aplica-o; sem braço no `presence`
# caía no `_ => ("?", "unsupported kind")`, visível aqui.
check "stack ls não diz 'unsupported kind' de um Kind que o apply aplica" ok \
  bash -c "! '$BIN' stack ls -f '$WORK/declarativos.yaml' | grep -q 'unsupported kind'"

########################################
section "container: quem grava o estado terminal, e a sonda de saúde"
########################################
# Três defeitos medidos a 2026-09-10 na varredura do grupo `container`, todos da
# mesma família: quem escreve o veredicto final sobre um container, e com que
# informação.
#
#   1. UM `stop` PEDIDO FICAVA `crashed`. O `runtime::stop` grava `Stopped`
#      "even if SIGKILL was needed"; o supervisor, que espera no MESMO processo,
#      via `Signaled` e escrevia `Crashed` por cima. Não é caso de fronteira: um
#      PID 1 sem handler de SIGTERM não morre com SIGTERM (o kernel só lho
#      entrega se houver handler), por isso o `stop` chega ao SIGKILL para o
#      container mais banal que existe — `alpine sleep 600`. Sintoma medido:
#      `ps -a` a dizer `Dead`, o `dash` a contá-lo em PROBLEMS como "killed by
#      signal (crash)", e o `wait` a responder 137, para um container que o
#      operador acabara de mandar parar.
#
#   2. O `start` PERDIA O SUPERVISOR. O `run -d` forka sempre um (é o que torna
#      o exit code de um container detached conhecível); o `start`/`restart` só
#      o faziam se houvesse política de restart. Medido: `run -d ... sh -c
#      'sleep 1; exit 7'` dava `wait` = 7, e depois de um `container start` do
#      MESMO container o `wait` recusava — a apontar para `--restart` como
#      remédio, quando o container tinha acabado de perder o supervisor que já
#      tinha. `ps -a` dizia `Exited (unknown)`.
#
#   3. O `healthcheck` IGNORAVA A SONDA DO CONTAINER. Lia só o `HEALTHCHECK` da
#      imagem, por isso um `run --health-cmd … alpine` mostrava `Up (healthy)`
#      no `ps` enquanto `container healthcheck <ele>` respondia "image
#      'alpine:3.20' defines no HEALTHCHECK" com rc=1 — vermelho permanente no
#      script de CI que é a razão de este verbo existir.
#
# E o quarto, que só se vê a contar processos: a sonda deixava DOIS zombies por
# execução (o subshell do watchdog, morto e nunca esperado, e o `sleep` dele,
# nunca sinalizado e órfão do PID 1 do container, que é o workload e não reapa
# nada). Medido com `--health-interval 2`: +8 a cada 10s, sem tecto, invisível
# ao `container top` (zombies não estão em `cgroup.procs`) e visível só como
# `pids.current` a subir. Com o `pids.max=512` por omissão, um container com
# healthcheck deixa de conseguir forkar em ~10 minutos a 2s de intervalo e em
# menos de três horas com o intervalo por omissão.
TSTOP="tstop-$PFX"; TWAIT="twait-$PFX"; THC="thc-$PFX"; TZ="tz-$PFX"

"$BIN" container run -d --name "$TSTOP" "$IMG" sleep 600 >/dev/null 2>&1
check "stop: uma paragem PEDIDA nunca fica 'crashed', mesmo tendo precisado de SIGKILL" ok bash -c "
  '$BIN' container stop -t 1 '$TSTOP' >/dev/null 2>&1
  # Janela generosa de propósito: o supervisor grava DEPOIS do stop, e é essa
  # segunda escrita que era o defeito. No binário defeituoso ela chegava em
  # menos de um segundo, sempre.
  st=
  for _ in \$(seq 1 30); do
    st=\$('$BIN' container inspect '$TSTOP' 2>/dev/null | python3 -c 'import json,sys;print(json.load(sys.stdin)[0][\"status\"])' 2>/dev/null)
    [ \"\$st\" = Crashed ] && break
    sleep 0.1
  done
  [ \"\$st\" = Stopped ] || { echo \"status=\$st (esperado Stopped)\"; '$BIN' container ps -a | grep '$TSTOP'; exit 1; }
"

check "start: o exit code REAL continua a ser capturado depois de um start" ok bash -c "
  '$BIN' container rm -f '$TWAIT' >/dev/null 2>&1
  '$BIN' container run -d --name '$TWAIT' '$IMG' /bin/sh -c 'sleep 1; exit 7' >/dev/null 2>&1
  primeiro=\$('$BIN' container wait '$TWAIT' 2>&1)
  [ \"\$primeiro\" = 7 ] || { echo \"run -d: wait deu '\$primeiro', esperado 7\"; exit 1; }
  '$BIN' container start '$TWAIT' >/dev/null 2>&1
  segundo=\$('$BIN' container wait '$TWAIT' 2>&1)
  [ \"\$segundo\" = 7 ] || { echo \"depois do start: wait deu '\$segundo', esperado 7\"; exit 1; }
"

"$BIN" container rm -f "$THC" >/dev/null 2>&1
"$BIN" container run -d --name "$THC" --health-cmd "test -f /tmp/ok" \
  --health-interval 2 --health-timeout 2 --health-retries 1 "$IMG" sleep 300 >/dev/null 2>&1
check "healthcheck: corre a sonda DO CONTAINER (unhealthy = rc 1, nunca 'no HEALTHCHECK')" 1 bash -c "
  out=\$('$BIN' container healthcheck '$THC' 2>&1); rc=\$?
  printf '%s\n' \"\$out\"
  printf '%s' \"\$out\" | grep -q 'defines no HEALTHCHECK' && { echo 'leu so a imagem'; exit 99; }
  exit \$rc
"
check "healthcheck: a mesma sonda a passar dá rc 0 e diz healthy" ok bash -c "
  '$BIN' container exec '$THC' /bin/sh -c 'touch /tmp/ok' >/dev/null 2>&1
  out=\$('$BIN' container healthcheck '$THC' 2>&1) || { echo \"\$out\"; exit 1; }
  printf '%s' \"\$out\" | grep -q healthy || { echo \"\$out\"; exit 1; }
"

"$BIN" container rm -f "$TZ" >/dev/null 2>&1
"$BIN" container run -d --name "$TZ" --health-cmd "true" --health-interval 1 \
  --health-timeout 1 "$IMG" sleep 300 >/dev/null 2>&1
if [ -n "$(_cg_of "$TZ" pids.current 2>/dev/null)" ]; then
  check "healthcheck: a sonda não deixa processos por reapar dentro do container" ok bash -c "
    antes=\$(_cg_of '$TZ' pids.current)
    sleep 12   # ~12 sondas a um intervalo de 1s
    depois=\$(_cg_of '$TZ' pids.current)
    # O container corre um \`sleep\` e mais nada: sem fuga, o número não se mexe.
    # Uma sonda a decorrer no instante da leitura explica 2, nunca uma dezena.
    [ \$((depois - antes)) -le 3 ] || { echo \"pids.current: \$antes → \$depois após ~12 sondas\"; exit 1; }
  "
else
  skip "healthcheck: a sonda não deixa processos por reapar" "cgroup do container ilegível (rootless sem delegação?)"
fi
TALW="talw-$PFX"
"$BIN" container rm -f "$TALW" >/dev/null 2>&1
"$BIN" container run -d --restart always --name "$TALW" "$IMG" sleep 600 >/dev/null 2>&1
check "stop: PÁRA mesmo um container --restart always (o supervisor não o ressuscita)" ok bash -c "
  '$BIN' container stop -t 1 '$TALW' >/dev/null 2>&1
  # O supervisor decide reiniciar depois do seu \`waitpid\`; 5s cobrem-no com
  # folga (medido no binário defeituoso: de volta a Running em ~4s, e o
  # \`stopped_by_user\` a ler \`false\` logo a seguir ao stop, apagado pelo
  # \`save\` do próprio stop).
  sleep 5
  st=\$('$BIN' container inspect '$TALW' 2>/dev/null | python3 -c 'import json,sys;print(json.load(sys.stdin)[0][\"status\"])' 2>/dev/null)
  [ \"\$st\" = Running ] && { echo 'ressuscitou: um stop pedido não parou o container'; exit 1; }
  [ \"\$st\" = Stopped ] || { echo \"status=\$st (esperado Stopped)\"; exit 1; }
"
"$BIN" container rm -f "$TSTOP" "$TWAIT" "$THC" "$TZ" "$TALW" >/dev/null 2>&1

########################################
section "schema gerado + explain + init"
########################################
check "manifest schema" ok "$BIN" manifest schema
check "manifest schema --kind Container" ok "$BIN" manifest schema --kind Container
check "manifest schema --kind inexistente recusa" fail "$BIN" manifest schema --kind NaoExiste
check "explain Kind" ok "$BIN" explain Container
check "explain campo" ok "$BIN" explain Container.ports
check "explain campo aninhado" ok "$BIN" explain Pod.containers.image
check "explain Kind inexistente recusa" fail "$BIN" explain NaoExiste
check "explain campo inexistente recusa" fail "$BIN" explain Container.naoExiste
# O schema publicado tem de ser o gerado — o mesmo contrato do teste em Rust,
# aqui contra o binário desta árvore.
#
# O ficheiro só existe se o script correr de dentro do checkout (deriva a raiz
# de `$0`). Correr de outro sítio — o caminho normal para exercitar a bateria
# numa VM descartável, que é onde ela SE consegue correr, já que os runners
# alojados bloqueiam userns — dava um FAIL cujo output era
# `diff: //docs/…: No such file or directory`. Isso não é «o schema divergiu»,
# é «não foi possível comparar», e confundir os dois é a mesma classe de erro
# que o resto deste motor persegue: uma medição que não se pôde fazer não é um
# resultado negativo.
SCHEMA_PUB="$(cd "$(dirname "$0")/.." && pwd)/docs/schema/v1/delonix.json"
if [[ -f "$SCHEMA_PUB" ]]; then
  check "schema publicado == gerado" ok bash -c \
    "'$BIN' manifest schema | diff -q - '$SCHEMA_PUB'"
else
  skip "schema publicado == gerado" "sem checkout à mão ($SCHEMA_PUB não existe)"
fi

INITDIR="$OUT/init-$PFX"; mkdir -p "$INITDIR"
check "init detecta e gera" ok "$BIN" init "$INITDIR"
check "init gerou um manifesto" ok test -f "$INITDIR/delonix-manifest.yaml"
check "o gerado valida" ok "$BIN" stack validate -f "$INITDIR/delonix-manifest.yaml"
# Sem `--force`, um segundo `init` não pode sobrescrever o que já lá está.
check "init repetido não sobrescreve" ok "$BIN" init "$INITDIR"

# O que um scaffold gera tem de APLICAR-SE. Medido antes de isto existir: o
# `vm init` produzia um `kind: Vm` com `network: <nome>-net` e nenhum
# `kind: Network` que o criasse, por isso o projecto falhava o seu PRÓPRIO
# `stack validate` («network … is not declared nor does it exist»). Um scaffold
# cujo primeiro acto é produzir algo que não aplica ensina a coisa errada sobre
# a ferramenta — e é um erro que só se vê correndo o comando, nunca lendo o
# `--help`, que é a lacuna que esta bateria tem por fechar.
#
# A lista abaixo diz `VirtualMachine` e não `Vm`: o Kind foi renomeado (com
# alias) e o scaffold passou a escrever a forma nova, mas este check continuou a
# procurar a antiga — verificava uma grafia que já ninguém gerava, e falhava por
# isso. Um check sobre drift do scaffold a fazer drift ele próprio.
VMINIT="$OUT/vminit-$PFX"; mkdir -p "$VMINIT"
if "$BIN" vm init "$VMINIT" --name "v$PFX" >/dev/null 2>&1; then
  check "vm init: o projecto que gera valida-se" ok \
    "$BIN" stack validate -f "$VMINIT/delonix-manifest.yaml"
  check "vm init: gera os três Kinds" ok bash -c "
    for k in Network Volume VirtualMachine; do
      grep -q \"kind: \$k\" '$VMINIT/delonix-manifest.yaml' || { echo \"falta kind: \$k\"; exit 1; }
    done
  "
else
  skip "vm init: projecto completo" "o vm init falhou"
fi

########################################
section "workload / pod / secret (leitura)"
########################################
check "workload ls" ok "$BIN" workload ls
check "workload ls -o json" ok "$BIN" workload ls -o json
check "workload describe inexistente recusa" fail "$BIN" workload describe "nao-existe-$PFX"
# `pod ls` foi colapsado em `get pods` no B7 (#159) — o grupo `pod` só
# tem create/logs/attach/cp/exec/port-forward.
check "get pods" ok "$BIN" get pods

# Um pod REAL de dois membros, pelo aviso que só um pod multi-membro revela.
#
# O aviso de cgroup é sobre a SESSÃO, não sobre um container, e o motor dedup'a-o
# com um `Once` — que vê UM processo. Cada membro de um pod entra por re-exec
# (`--pod`), logo é o seu próprio processo e recomeça o `Once`: o mesmo bloco de
# oito linhas saía uma vez POR MEMBRO (medido: 3× num pod de 3).
#
# «No máximo um», e não «exactamente um», de propósito: num host COM delegação de
# cgroup não há aviso nenhum e exigir 1 falharia ali por razão errada. O que nunca
# pode voltar é a repetição por membro — que é o que um pod de 2 já expõe.
PODY="$OUT/pod-$PFX.yaml"
cat >"$PODY" <<YAML
apiVersion: delonix.io/v1
kind: Pod
metadata:
  name: p$PFX
spec:
  containers:
    - name: web
      image: $IMG
      command: ["sleep", "120"]
    - name: api
      image: $IMG
      command: ["sleep", "120"]
YAML
if "$BIN" pod create -f "$PODY" >/dev/null 2>"$OUT/pod-$PFX.err"; then
  check "pod create: o aviso de cgroup não se repete por membro" ok bash -c "
    n=\$(grep -c 'cgroup delegation' '$OUT/pod-$PFX.err' || true)
    [ \"\$n\" -le 1 ] || { echo \"o aviso saiu \$n vezes (um por membro)\"; exit 1; }
  "
  check "get pods mostra-o" ok bash -c "'$BIN' get pods | grep -q 'p$PFX'"

  # `pod exec`/`pod cp`/`pod attach` — wrappers finos sobre `container exec/cp/
  # attach`, que resolvem `--container <curto>` (ou o 1.º membro por omissão)
  # para o nome real `<pod>-<membro>` no Store. Provados com uma escrita
  # POR MEMBRO (não `hostname`: os membros partilham UTS, por isso um
  # `hostname` igual não provaria que o `--container` escolheu o certo — só a
  # mountns, que NÃO é partilhada, distingue).
  # Os membros chamam-se `web` e `api`, nesta ordem, e a escolha NÃO é
  # decorativa: a ordem do MANIFESTO tem de ser CONTRÁRIA à alfabética.
  # Este bloco testava com `a`/`b`, que é precisamente o par que não distingue
  # as correcções possíveis — ordenar por NOME teria passado aqui por acidente e
  # deixado um pod `web`/`api` real com o mesmo defeito. Com estes nomes, tanto
  # o bug como a meia-correcção põem o `api` no lugar do `web`. (O desenho é do
  # #257, que chegou a ele por outro caminho; salvado aqui porque valia mais do
  # que o que estava.)
  #
  # A prova do `--container` é feita com o `--container` EXPLÍCITO nas duas
  # pontas: escreve-se em `web`, lê-se em `web` (tem de estar lá) e lê-se em
  # `api` (não pode estar). Isto isola a pergunta «o `--container` escolhe o
  # membro certo, e a mountns não é partilhada» da pergunta sobre o membro por
  # OMISSÃO, abaixo.
  check "pod exec --container web escreve em web" ok bash -c \
    "'$BIN' pod exec p$PFX --container web sh -c 'echo do-web > /tmp/mark-$PFX'"
  check "pod exec --container web relê o que escreveu" ok bash -c \
    "'$BIN' pod exec p$PFX --container web cat /tmp/mark-$PFX | grep -q do-web"
  check "'api' não vê a escrita de 'web' (mountns própria)" fail \
    "$BIN" pod exec "p$PFX" --container api cat "/tmp/mark-$PFX"

  # O MEMBRO POR OMISSÃO — e agora com o FACTO medido ao lado do comportamento.
  #
  # História, porque explica os dois checks: `resolve_target` prometia «the
  # pod's first member when omitted» e devolvia o primeiro que o `Store::list`
  # desse. O `Store::list` ordena por `Reverse(created_unix)` — SEGUNDOS —, dois
  # membros criados no mesmo segundo empatam, e o desempate acabava por ser a
  # ordem do `read_dir`. Ordem de sistema de ficheiros, não a do manifesto.
  #
  # Isto esteve aqui marcado `xflaky ACH-011` durante algumas horas, precisamente
  # porque a natureza do defeito era a intermitência: numa raiz virgem o default
  # caía em `web` e passava; na raiz da bateria completa caía em `api`, e os
  # checks vizinhos chumbavam a acusar o `--container` de escolher mal, que é o
  # oposto do defeito.
  #
  # A marca SAIU porque o defeito foi corrigido (#257/#258: um rótulo de posição
  # carimbado no `pod create`, e `members_of` a ordenar por ele). Medido na `main`
  # com seis containers de ruído no store — a condição em que batia — 6/6
  # resoluções em `web`. É o ciclo do ratchet fechado: a marca é para o intervalo
  # entre descobrir e corrigir, e um XPASS teria chumbado o portão a pedi-la de
  # volta se eu me tivesse esquecido.
  #
  # DOIS checks e não um. O comportamento depende da ordem que o store devolve;
  # o rótulo não. Sem a correcção o rótulo não existe DE TODO, por isso o
  # primeiro check apanha a regressão em TODAS as corridas, e não em metade.
  check "pod create carimba a posição de cada membro" ok bash -c \
    "'$BIN' container inspect p$PFX-web | grep -q 'pod-index\": \"0\"' && \
     '$BIN' container inspect p$PFX-api | grep -q 'pod-index\": \"1\"'"
  check "pod exec sem --container vai ao 1.º membro DECLARADO" ok bash -c \
    "'$BIN' pod exec p$PFX sh -c 'echo do-default > /tmp/mk2-$PFX' && \
     '$BIN' pod exec p$PFX --container web cat /tmp/mk2-$PFX | grep -q do-default"
  check "pod exec --container api escreve só em api" ok bash -c \
    "'$BIN' pod exec p$PFX --container api sh -c 'echo do-api > /tmp/mark2-$PFX'"
  check "pod exec --container api confirma" ok bash -c \
    "'$BIN' pod exec p$PFX --container api cat /tmp/mark2-$PFX | grep -q do-api"
  check "pod exec --container inexistente recusa" fail \
    "$BIN" pod exec "p$PFX" --container nope true

  check "pod cp: host -> 1.º membro" ok bash -c \
    "echo prova-podcp > '$OUT/podcp-$PFX.txt' && '$BIN' pod cp '$OUT/podcp-$PFX.txt' p$PFX:/tmp/podcp-$PFX.txt"
  check "pod cp: chegou ao 1.º membro" ok bash -c \
    "'$BIN' pod exec p$PFX cat /tmp/podcp-$PFX.txt | grep -q prova-podcp"
  check "pod cp --container escolhe o membro" ok bash -c \
    "'$BIN' pod cp '$OUT/podcp-$PFX.txt' p$PFX:/tmp/podcp2-$PFX.txt --container api && \
     '$BIN' pod exec p$PFX --container api cat /tmp/podcp2-$PFX.txt | grep -q prova-podcp"
  check "pod cp: membro -> host" ok bash -c \
    "'$BIN' pod cp p$PFX:/tmp/podcp-$PFX.txt '$OUT/podcp-back-$PFX.txt' && grep -q prova-podcp '$OUT/podcp-back-$PFX.txt'"

  # `pod attach` segue os logs (`follow=true`) — contra um membro vivo isso
  # bloqueia para sempre; o `timeout` + rc=124 prova que ligou e ficou a seguir,
  # não que falhou logo. `-i` tem de recusar de imediato (mesmo contrato do
  # `container attach`).
  check "pod attach bloqueia (segue o output de um membro vivo)" 124 \
    timeout 2 "$BIN" pod attach "p$PFX"
  check "pod attach -i recusa (sem stdin ao vivo)" fail \
    "$BIN" pod attach "p$PFX" -i

  # `pod describe`/`pod rm` NÃO existem como subcomandos próprios — o B7
  # (#159) colapsou-os nos verbos genéricos por-Kind. Medido ao vivo antes de
  # escrever este fix: `delonix pod describe`/`delonix pod rm` respondem
  # `unrecognized subcommand` desde essa PR, e este script continuava a
  # chamá-los — achado do mesmo tipo do `volumes`/`volume` da sessão anterior.
  # `delonix vm rm` tinha a mesma quebra em vários pontos deste script — ver
  # `delete vm`/`describe vm` — e foi corrigido numa sessão à parte.
  check "describe pod" ok "$BIN" describe pod "p$PFX"
  check "delete pod -f" ok "$BIN" delete pod "p$PFX" -f
else
  skip "pod create + aviso de cgroup" "o pod create falhou (holder/SDN indisponível)"
fi

# As portas de um membro de pod vivem no ingress partilhado, e o registo de um
# membro não tem `network` (a pertença é o campo `pod`). O `stop`/`rm` só olhavam
# para `network`: o hostfwd ficava no ingress depois do `rm -f`, e um pod novo na
# mesma porta era recusado com «already in use». E o `start` não republicava.
PPORT=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])')
PPY="$OUT/podport-$PFX.yaml"
cat > "$PPY" <<YAML
apiVersion: compute.delonix.io/v1alpha1
kind: Pod
metadata: { name: pp$PFX }
spec:
  containers:
    - name: web
      image: $IMG
      command: ["sh", "-c", "while true; do printf 'HTTP/1.0 200 OK\\\\r\\\\n\\\\r\\\\nhi\\\\n' | nc -l -p 8080; done"]
      ports: [{ containerPort: 8080, hostPort: $PPORT }]
YAML
pp_code() { curl -s -m 5 -o /dev/null -w '%{http_code}' "http://127.0.0.1:$PPORT/"; }
pp_wait() {  # $1 = código esperado
  local i
  for i in $(seq 1 30); do [ "$(pp_code)" = "$1" ] && return 0; sleep 0.5; done
  return 1
}
if "$BIN" pod create -f "$PPY" >/dev/null 2>&1 && pp_wait 200; then
  "$BIN" container stop -t 1 "pp$PFX-web" >/dev/null 2>&1
  check "o stop de um membro de pod liberta a porta" ok bash -c "[ -z \"\$(ss -tlnH 'sport = :$PPORT')\" ]"
  "$BIN" container start "pp$PFX-web" >/dev/null 2>&1
  check "o start de um membro de pod volta a publicar a porta" ok bash -c \
    "for _ in \$(seq 1 30); do [ \"\$(curl -s -m 5 -o /dev/null -w '%{http_code}' http://127.0.0.1:$PPORT/)\" = 200 ] && exit 0; sleep 0.5; done; exit 1"
  "$BIN" container rm -f "pp$PFX-web" >/dev/null 2>&1
  check "o rm -f de um membro de pod liberta a porta" ok bash -c "[ -z \"\$(ss -tlnH 'sport = :$PPORT')\" ]"
  check "e um pod novo na mesma porta é aceite" ok "$BIN" pod create -f "$PPY"
  # Publicar a QUENTE num membro também passa pelo ingress. Era recusado com
  # «created without `-p` and without `--net`», que não era a razão.
  HPORT=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])')
  check "container update --publish-add num membro de pod" ok "$BIN" container update --publish-add "$HPORT:8080" "pp$PFX-web"
  check "e a porta publicada a quente responde" ok bash -c \
    "for _ in \$(seq 1 30); do [ \"\$(curl -s -m 5 -o /dev/null -w '%{http_code}' http://127.0.0.1:$HPORT/)\" = 200 ] && exit 0; sleep 0.5; done; exit 1"
  check "container update --publish-rm num membro de pod" ok "$BIN" container update --publish-rm "$HPORT" "pp$PFX-web"
  check "e a porta deixa de escutar" ok bash -c "[ -z \"\$(ss -tlnH 'sport = :$HPORT')\" ]"
  "$BIN" container rm -f "pp$PFX-web" >/dev/null 2>&1
else
  skip "portas de um membro de pod" "o pod create com porta falhou (holder/SDN indisponível)"
fi
check "secret ls" ok "$BIN" secret ls
check "secret inspect inexistente recusa" fail "$BIN" secret inspect "nao-existe-$PFX"

# `secret create` sobre um nome que JÁ existe: até 2026-09-10 substituía o
# segredo inteiro em silêncio (`created (1 key(s))` sobre uma substituição real
# — lia-se como «criei um segredo novo», não «duas credenciais desapareceram»).
# Corrigido nesse dia para pelo menos avisar (ACH-024..027, #278). Desde
# 2026-09-12 (Sprint 1 da reestruturação da CLI) `create` deixou de substituir
# por omissão de todo — só cria, a mesma garantia que `kubectl create secret`/
# `docker secret create` dão — e exige `--force` para o comportamento antigo.
# `secret set`/`secret apply` (o caminho declarativo, que TEM de continuar a
# substituir — é o que aplicar um manifesto significa) ficam de fora desta
# recusa de propósito: só o `create` imperativo pede intenção explícita.
SEC="sec-$PFX"
check "secret create: um nome novo diz 'created'" ok bash -c "
  '$BIN' secret create '$SEC' --from-literal a=1 --from-literal b=2 2>&1 | grep -q created"
check "secret create: por cima de um existente RECUSA sem --force" 5 "$BIN" secret create "$SEC" --from-literal c=3
check "secret create: a recusa não mexeu em nada" ok bash -c "
  keys=\$('$BIN' secret inspect '$SEC' 2>&1)
  printf '%s' \"\$keys\" | grep -q 'a=' && printf '%s' \"\$keys\" | grep -q 'b=' || exit 1
  printf '%s' \"\$keys\" | grep -q 'c=' && exit 1
  exit 0"
check "secret create --force: por cima de um existente diz 'replaced' e NOMEIA o que largou" ok bash -c "
  out=\$('$BIN' secret create '$SEC' --force --from-literal c=3 2>&1)
  printf '%s\n' \"\$out\"
  printf '%s' \"\$out\" | grep -q replaced || exit 1
  printf '%s' \"\$out\" | grep -q 'a, b' || exit 1"
check "secret create --force: e as chaves largadas foram MESMO largadas" ok bash -c "
  keys=\$('$BIN' secret inspect '$SEC' 2>&1)
  printf '%s' \"\$keys\" | grep -q 'c=' || exit 1
  printf '%s' \"\$keys\" | grep -qE '(^|[^a-z])a=' && exit 1
  exit 0"
check "secret set continua não-destrutivo por cima de um existente" ok bash -c "
  '$BIN' secret set '$SEC' d=4 >/dev/null 2>&1
  keys=\$('$BIN' secret inspect '$SEC' 2>&1)
  printf '%s' \"\$keys\" | grep -q 'c=' && printf '%s' \"\$keys\" | grep -q 'd=' || exit 1
  exit 0"
"$BIN" secret rm "$SEC" >/dev/null 2>&1

########################################
section "vm (só o que não precisa de hipervisor)"
########################################
check "vm ls" ok "$BIN" vm ls
# `--disk`, e não `--image`: a flag `--image` NÃO EXISTE no `vm create`, por isso
# este check passava — esperava falha e obtinha falha — mas por «unexpected
# argument», nunca por a imagem não existir. Um teste que passa pela razão errada
# é pior que um teste em falta: dá cobertura por adquirida.
check "vm create com disco inexistente recusa" fail "$BIN" vm create "vm-$PFX" --disk /nao/existe.qcow2
# ADR-0050 D6: a requirement by capability name. An unknown name is an invalid
# argument (1) BEFORE any backend is asked; a backend whose report does not
# mark the entry usable is refused in the UNAVAILABLE class (69) before the disk
# is touched — which is why a disk that does not exist is fine here: the
# refusal has to come first, or it is not "before anything is created".
check "vm create --require com nome desconhecido recusa (1)" 1 "$BIN" vm create "vm-$PFX-req" --disk /nao/existe.qcow2 --require vm.snapshot.memry
check "vm create --require de capacidade que o CH não tem recusa (69)" 69 "$BIN" vm create "vm-$PFX-req" --disk /nao/existe.qcow2 --backend cloud-hypervisor --require vm.snapshot.memory
check "vm create --require de capacidade que o libvirt não tem recusa (69)" 69 "$BIN" vm create "vm-$PFX-req" --disk /nao/existe.qcow2 --backend libvirt --require vm.namespace-isolation
check "vm create --require: a recusa nomeia a capacidade e o estado" ok bash -c "\"$BIN\" vm create vm-$PFX-req --disk /nao/existe.qcow2 --backend libvirt --require vm.namespace-isolation 2>&1 | grep -q 'vm.namespace-isolation: unsupported-by-provider'"
check "vm create --require: nenhum registo de VM ficou para trás" fail "$BIN" vm inspect "vm-$PFX-req"

########################################
section "vm: o snapshot sobrevive a um stop/start (precisa de hipervisor)"
########################################
# BUG REAL, medido 2026-08-12: `vm stop` desfaz o domínio com `virsh undefine
# --snapshots-metadata` (é o que evita domínios órfãos), e isso apagava os
# METADADOS dos snapshots — `vm snapshots` respondia VAZIO com rc=0 e o
# `vm restore` dizia «Domain snapshot not found», com o estado do snapshot
# intacto dentro do qcow2 o tempo todo (`qemu-img snapshot -l` mostrava-o).
#
# O gate tem de olhar para o CICLO — snapshot, stop, start, restore — e não
# para o rc de cada comando: antes da correcção TODOS eles devolviam 0. E a
# recusa com a VM parada verifica-se pela MENSAGEM, não pelo código de saída:
# o erro cru do virsh também saía 1, logo um `fail` ficaria verde por cima do
# relato errado.
# O caminho do overlay vem do PRÓPRIO motor, nunca de um default assumido aqui.
SROOT=$("$BIN" system info 2>/dev/null | awk '/state root:/{print $3}')

if command -v virsh >/dev/null && command -v qemu-img >/dev/null \
   && virsh -c qemu:///system list --all >/dev/null 2>&1; then
  SVM="snap-$PFX"; SDISK="$OUT/$SVM.qcow2"
  qemu-img create -f qcow2 "$SDISK" 64M >/dev/null 2>&1
  # `--require vm.snapshot.memory`: the accepted half of ADR-0050 D6 — libvirt
  # marks it usable on a host with qemu:///system, so the requirement passes and
  # the create goes ahead exactly as before (the refusals are in the vm section).
  if "$BIN" vm create "$SVM" --disk "$SDISK" --backend libvirt --memory 256M --require vm.snapshot.memory >/dev/null 2>&1; then
    check "vm snapshot create" ok "$BIN" vm snapshot create "$SVM" s1
    check "vm snapshot ls nomeia-o com a VM a correr" ok bash -c \
      "'$BIN' vm snapshot ls '$SVM' | grep -qx s1"
    check "vm stop" ok "$BIN" vm stop "$SVM"
    check "o snapshot sobrevive ao stop" ok bash -c \
      "'$BIN' vm snapshot ls '$SVM' | grep -qx s1"
    check "vm start" ok "$BIN" vm start "$SVM"
    check "o libvirt volta a conhecer o snapshot" ok bash -c \
      "virsh -c qemu:///system snapshot-list '$SVM' --name | grep -qx s1"
    check "vm snapshot restore depois do start" ok "$BIN" vm snapshot restore "$SVM" s1

    # BUG REAL, reproduzido 2026-09-15: `vm pause` devolvia 0 e o libvirt dizia
    # `paused`, mas o `vm ls` seguinte reportava `Stopped` — só `running`
    # contava como vivo, e a guarda do Paused está no ramo «vivo». O rc do
    # pause não prova nada: o que se lê é o REGISTO, e duas vezes, porque é a
    # reconciliação de um `ls` que o estragava.
    #
    # `-A`, e não é enfeite: desde o #326 o `vm ls` sem ele mostra SÓ o que está
    # `Running` (o mesmo corte do `docker ps`). Sem o `-A` uma VM Paused ou
    # Stopped nunca aparece na lista, e os três checks abaixo que perguntam por
    # esses estados chumbavam SEMPRE, com a saída vazia — um vermelho fixo desde
    # 2026-09-16 (#326) que se lia como intermitente por estar ao lado da secção CH.
    vm_status_is() {
      "$BIN" vm ls -A -o json | python3 -c "import json,sys; sys.exit(0 if any(v['name']==sys.argv[1] and v['status']==sys.argv[2] for v in json.load(sys.stdin)) else 1)" "$1" "$2"
    }
    check "vm pause" ok "$BIN" vm pause "$SVM"
    check "o libvirt confirma paused" ok bash -c \
      "[ \"\$(virsh -c qemu:///system domstate '$SVM')\" = paused ]"
    check "o vm ls diz Paused (não Stopped)" ok vm_status_is "$SVM" Paused
    check "e continua Paused num segundo ls" ok vm_status_is "$SVM" Paused
    # Sem -A o `vm ls` mostra o que está de pé, e uma VM pausada está de pé.
    check "o vm ls sem -A mostra a VM pausada" ok bash -c \
      "'$BIN' vm ls -o json | python3 -c \"import json,sys; sys.exit(0 if any(v['name']=='$SVM' for v in json.load(sys.stdin)) else 1)\""
    check "vm unpause" ok "$BIN" vm unpause "$SVM"
    check "o vm ls volta a dizer Running" ok vm_status_is "$SVM" Running
    # Parar uma VM PAUSADA: o domínio tem de ir embora e o registo dizer Stopped.
    check "vm pause (antes do stop)" ok "$BIN" vm pause "$SVM"
    check "vm stop de uma VM pausada" ok "$BIN" vm stop "$SVM"
    check "o domínio pausado foi desfeito" ok bash -c \
      "! virsh -c qemu:///system domstate '$SVM' >/dev/null 2>&1"
    check "e o vm ls diz Stopped" ok vm_status_is "$SVM" Stopped
    check "vm start depois do stop de uma VM pausada" ok "$BIN" vm start "$SVM"

    # Com a VM PARADA os três verbos continuam a funcionar — o domínio libvirt
    # não existe nesse estado, e é definido só o tempo do comando.
    check "vm stop (2.ª vez)" ok "$BIN" vm stop "$SVM"
    check "snapshot create com a VM parada" ok "$BIN" vm snapshot create "$SVM" s2
    check "e a VM CONTINUA parada" ok bash -c \
      "! virsh -c qemu:///system domstate '$SVM' >/dev/null 2>&1"
    check "o novo aparece no ls" ok bash -c \
      "'$BIN' vm snapshot ls '$SVM' | grep -qx s2"
    check "restore de um snapshot offline não arranca a VM" ok bash -c \
      "'$BIN' vm snapshot restore '$SVM' s2 && ! virsh -c qemu:///system domstate '$SVM' >/dev/null 2>&1"
    # s1 foi tirado com a VM a correr: restaurá-lo TEM de a trazer de volta.
    check "restore de um snapshot vivo traz a VM de volta a correr" ok bash -c \
      "'$BIN' vm snapshot restore '$SVM' s1 && '$BIN' vm ls -o json | python3 -c \"import json,sys; sys.exit(0 if any(v['name']=='$SVM' and v['status']=='Running' for v in json.load(sys.stdin)) else 1)\""
    check "vm snapshot rm" ok "$BIN" vm snapshot rm "$SVM" s2
    # Sair da LISTA não é sair do disco: o `qemu-img` é a única testemunha.
    check "e sai mesmo do disco, não só da lista" ok bash -c \
      "! qemu-img snapshot -l '$SROOT/vms/$SVM.qcow2' 2>/dev/null | grep -qw s2 && ! '$BIN' vm snapshot ls '$SVM' | grep -qx s2"

    # A CLASSE da falha, que é o que um reconciliador lê (docs/cli-stability.md).
    check "restore de um snapshot inexistente diz 4" 4 "$BIN" vm snapshot restore "$SVM" naoexiste
    check "rm de um snapshot inexistente diz 4" 4 "$BIN" vm snapshot rm "$SVM" naoexiste
    check "create com nome já usado diz 5 (conflito)" 5 "$BIN" vm snapshot create "$SVM" s1
    # A quebra da v0.51.x tem de falhar ALTO, nunca em silêncio.
    check "a forma antiga 'vm snapshots' já não existe" fail "$BIN" vm snapshots "$SVM"
    check "a forma antiga 'vm restore' já não existe" fail "$BIN" vm restore "$SVM" s1
    "$BIN" delete vm "$SVM" -f >/dev/null 2>&1
  else
    skip "vm: snapshot sobrevive a stop/start" "o vm create falhou neste host"
    "$BIN" delete vm "$SVM" -f >/dev/null 2>&1
  fi

  rm -f "$SDISK"
else
  skip "vm: snapshot sobrevive a stop/start" "sem virsh/qemu-img, ou sem ligação libvirt de sistema"
fi

########################################
section "vm: os mesmos snapshots no backend cloud-hypervisor"
########################################
# Aqui os snapshots são do disco (`qemu-img snapshot`) e SÓ com a VM parada: o
# vmm a correr segura o qcow2 em exclusivo e o CH não tem API de snapshot de
# disco ao vivo (a `vm.snapshot` dele guarda memória+dispositivos e NÃO o disco
# — restaurá-la contra um disco que andou não é voltar atrás). O que este bloco
# prova é que a recusa é CLARA e que os quatro verbos funcionam com a VM parada
# — nunca um silêncio. Precisa do holder de rede a correr (o vmm do CH vive lá
# dentro), por isso salta em vez de falhar quando o `create` não passa.
#
# MEIA-ISOLAÇÃO É PIOR QUE NENHUMA, e custou um incidente real (2026-08-12):
# `DELONIX_ROOT` isolado SEM `DELONIX_NET_RUNTIME_DIR` deixa os dois roots a
# partilhar `/tmp/delonix-net-<uid>/{control,slirp}.sock` — os sockets são por
# UTILIZADOR e os pidfiles por ROOT. O motor tem um guarda que recusa isso, mas
# ele deixa de disparar assim que o root isolado ganha estado de ingress
# próprio: a partir daí sobe um pin/slirp SEUS por cima dos mesmos caminhos, e
# o `net netns up` seguinte, corrido do root REAL, encontra o controlo partido,
# reconstrói tudo e reinicia containers de produção. Aqui recusa-se a correr
# nessa configuração em vez de a exercitar.
if [[ -n "${DELONIX_ROOT:-}" && -z "${DELONIX_NET_RUNTIME_DIR:-}" ]]; then
  skip "vm: snapshots no cloud-hypervisor" \
    "DELONIX_ROOT isolado sem DELONIX_NET_RUNTIME_DIR — isola os DOIS ou nenhum"
elif command -v cloud-hypervisor >/dev/null; then
  CVM="chsnap-$PFX"; CDISK="$OUT/$CVM.qcow2"
  qemu-img create -f qcow2 "$CDISK" 64M >/dev/null 2>&1
  if "$BIN" vm create "$CVM" --disk "$CDISK" --backend cloud-hypervisor --memory 256M >/dev/null 2>&1; then
    # A PRÉ-CONDIÇÃO dos checks «com a VM a correr», verificada e não presumida.
    #
    # Medido 2026-09-16: dentro da bateria completa esta secção chumbava 2 a 4
    # checks em CASCATA — «rm com a VM a correr RECUSA» com rc=0, e um «create
    # com a VM parada» a dar 5 porque o `s1` já tinha sido criado pelo check
    # «e a recusa diz o que fazer», que corre o MESMO `snapshot create` e o viu
    # passar. Nas duas pontas a recusa não disparou porque não havia VMM: o
    # `qemu-img` conseguiu o lock de escrita do qcow2, e só consegue com o
    # processo morto. Sozinha, com o mesmo binário, a secção dava 12/12.
    #
    # NÃO é o disco vazio, e isto foi medido porque era a suspeita óbvia: o
    # `CLOUDHV.fd` sobre um qcow2 vazio não sai — fica em `BdsDxe: No bootable
    # option or device was found` (120 s cronometrados, processo vivo), 25 de 25
    # VMs criadas em laço a load 41 estavam vivas 3 s depois, e duas baterias
    # completas (debug e release) com um poller a 20 ms viram o VMM em `S` até
    # cada `vm stop`, sem uma morte. A causa da morte ficou POR MEDIR — não se
    # reproduziu à ordem — e é por isso que a guarda abaixo, quando dispara,
    # despeja o log do VMM em vez de só dizer que ele não está lá.
    #
    # O que o check prova é «a recusa acontece com a VM VIVA». Contra uma VM
    # morta não prova nada — e pior, muda o disco e envenena os checks
    # seguintes, que é exactamente como um VMM ausente virava quatro vermelhos
    # que se liam como defeitos do `snapshot`. Por isso cada check que depende
    # de o VMM estar vivo é emoldurado por `ch_live`:
    #
    #   - ANTES: há um processo `cloud-hypervisor` com o api-socket DESTA VM (a
    #     testemunha independente do motor) E o `vm ls` do motor diz Running.
    #     Se não, o check NÃO corre — não mexe no disco — e o que chumba é a
    #     pré-condição, com nome próprio e a cauda do log do VMM, que é onde
    #     está a razão da morte.
    #   - DEPOIS: o VMM continua lá. Um processo que morre A MEIO do check
    #     deixa a recusa por provar, e isso também chumba com nome próprio.
    #
    # Não enfraquece nada: a recusa continua a ser exercida contra um VMM vivo,
    # e agora isso é afirmado dos dois lados em vez de assumido.
    ch_vmm_up() {
      local sock="$SROOT/vms/$CVM.sock" why=""
      pgrep -f -- "^(\S*/)?cloud-hypervisor .*--api-socket $sock( |\$)" >/dev/null \
        || why="nenhum processo cloud-hypervisor com --api-socket $sock"
      if [[ -z "$why" ]] && ! "$BIN" vm ls -o json | python3 -c \
          "import json,sys; sys.exit(0 if any(v['name']==sys.argv[1] and v['status']=='Running' for v in json.load(sys.stdin)) else 1)" "$CVM"; then
        why="o processo existe mas o vm ls não diz Running"
      fi
      [[ -z "$why" ]] && return 0
      echo "VMM de '$CVM' em baixo: $why"
      echo "--- cauda de $SROOT/vms/$CVM.log:"
      grep -v DEPRECATION "$SROOT/vms/$CVM.log" 2>/dev/null | tail -6
      return 1
    }
    ch_live() {
      local name="$1"; shift
      if ! ch_vmm_up >/dev/null 2>&1; then
        check "$name — pré-condição: VMM vivo ANTES (o check não correu)" ok ch_vmm_up
        return
      fi
      check "$name" "$@"
      ch_vmm_up >/dev/null 2>&1 \
        || check "$name — pré-condição: VMM vivo DEPOIS (o VMM morreu a meio)" ok ch_vmm_up
    }
    ch_live "CH: create com a VM a correr RECUSA" fail "$BIN" vm snapshot create "$CVM" s1
    ch_live "CH: e a recusa diz o que fazer" ok bash -c \
      "'$BIN' vm snapshot create '$CVM' s1 2>&1 | grep -q 'vm stop'"
    check "CH: vm stop" ok "$BIN" vm stop "$CVM"
    check "CH: create com a VM parada" ok "$BIN" vm snapshot create "$CVM" s1
    check "CH: ls nomeia-o" ok bash -c "'$BIN' vm snapshot ls '$CVM' | grep -qx s1"
    check "CH: restore" ok "$BIN" vm snapshot restore "$CVM" s1
    check "CH: create repetido diz 5" 5 "$BIN" vm snapshot create "$CVM" s1
    check "CH: restore de inexistente diz 4" 4 "$BIN" vm snapshot restore "$CVM" naoexiste
    check "CH: vm start" ok "$BIN" vm start "$CVM"
    # O snapshot vive no disco, por isso sobrevive por construção — e o `ls`
    # tem de responder mesmo com o vmm a segurar o ficheiro (`qemu-img info -U`).
    ch_live "CH: o ls responde com a VM a correr" ok bash -c \
      "'$BIN' vm snapshot ls '$CVM' | grep -qx s1"
    ch_live "CH: rm com a VM a correr RECUSA" fail "$BIN" vm snapshot rm "$CVM" s1
    "$BIN" vm stop "$CVM" >/dev/null 2>&1
    # ESPERA PELA CONDIÇÃO, e a condição é o lock do qcow2 — não o `stop`.
    #
    # `CloudHypervisor::stop` (delonix-vm/src/lib.rs) manda SIGTERM e devolve
    # SEM esperar que o VMM saia. O `snapshot rm` que vem a seguir passa pelo
    # `qemu-img`, que precisa do lock de escrita do disco, e uma corrida de cinco
    # hoje (2026-09-09, load ~18) apanhou o VMM ainda vivo:
    #
    #     qemu-img: Could not open '…qcow2': Failed to lock byte 100
    #
    # NÃO vai marcado como defeito do produto: não o consegui reproduzir à ordem
    # (0 falhas em 20 tentativas apertadas a load 22), e uma marca sobre uma
    # ocorrência só afirma mais do que se mediu. Está aberto como ACH-014.
    #
    # O que vai é a pré-condição explícita. Este check mede se o `snapshot rm`
    # APAGA o snapshot; se o `stop` é síncrono é outra pergunta, e misturá-las
    # dava um vermelho intermitente que se lê como defeito do `snapshot rm`.
    # O `qemu-img snapshot -l` SEM `-U` pede o mesmo lock que o `rm` vai pedir,
    # por isso é a condição exacta e não um proxy dela — DESDE QUE seja no
    # overlay. Até 2026-09-16 perguntava ao `$CDISK`, o disco base que se passou
    # ao `create`, e esse o VMM nunca tranca: medido com um CH vivo, o base dá
    # rc=0 e o overlay `Failed to lock byte 100`. O ciclo saía à primeira e não
    # esperava por nada; o lock que o `rm` pede é o de `$SROOT/vms/$CVM.qcow2`.
    for _ in $(seq 50); do
      qemu-img snapshot -l "$SROOT/vms/$CVM.qcow2" >/dev/null 2>&1 && break
      sleep 0.2
    done
    check "CH: rm com a VM parada" ok "$BIN" vm snapshot rm "$CVM" s1
    check "CH: e saiu do disco" ok bash -c \
      "! qemu-img snapshot -l '$SROOT/vms/$CVM.qcow2' 2>/dev/null | grep -qw s1"
    "$BIN" delete vm "$CVM" -f >/dev/null 2>&1
  else
    skip "vm: snapshots no cloud-hypervisor" "o vm create CH falhou neste host (infra de rede?)"
    "$BIN" delete vm "$CVM" -f >/dev/null 2>&1
  fi
  rm -f "$CDISK"
else
  skip "vm: snapshots no cloud-hypervisor" "sem cloud-hypervisor instalado"
fi

########################################
section "backup / restore por recurso"
########################################
# O ciclo real: arquivar, DESTRUIR os dados, repor, e confirmar que voltaram. Um
# `backup` que devolve 0 não prova nada — o que prova é o conteúdo do ficheiro
# depois de ele ter sido apagado.
# Fresh per run: `$OUT` survives between runs, and a leftover archive would make
# the "--dry-run wrote nothing" check fail for a reason that has nothing to do
# with the code.
BKDIR="$OUT/backups"; rm -rf "$BKDIR"; mkdir -p "$BKDIR"
BKC="bk-$PFX"; BKV="bkvol-$PFX"
"$BIN" volume create "$BKV" >/dev/null 2>&1
"$BIN" container run -d --name "$BKC" -v "$BKV":/data alpine:latest sleep 300 >/dev/null 2>&1
# Esperar que a escrita PERSISTA, e não que o comando devolva 0.
#
# Medido a 2026-08-28, seis ciclos: em DOIS deles um `exec` disparado logo a
# seguir ao `run -d` escreveu e o `cat` seguinte não encontrou nada. O `run -d`
# devolve antes de o volume estar montado, e o `exec` que apanha essa janela
# escreve para o `/data` do rootfs em vez de para o volume — devolvendo 0.
#
# É a causa REAL da falha intermitente de «os dados voltaram», que este ficheiro
# já registava desde 2026-08-25 e atribuía ao restore. Não era o restore: nunca
# havia dados para repor. Um backup de um volume vazio restaura um volume vazio,
# e o check chumbava três passos depois do sítio onde o problema estava.
#
# E é um CHECK, não um ciclo mudo. O ciclo anterior fazia `break` no sucesso e
# CAÍA em silêncio depois de 50 tentativas: se a escrita nunca pegasse, a bateria
# seguia, arquivava um volume vazio, e o vermelho aparecia três checks à frente
# em «os dados voltaram» — que é EXACTAMENTE a misdiagnose que este comentário
# diz ter corrigido, ainda possível. Uma pré-condição que falha em silêncio move
# o vermelho para longe da causa, que é a única coisa que um relatório não pode
# fazer.
check "a escrita no volume PERSISTIU (pré-condição do backup)" ok bash -c \
  "for _ in \$(seq 50); do
     '$BIN' container exec '$BKC' sh -c 'echo prova > /data/f.txt' >/dev/null 2>&1
     '$BIN' container exec '$BKC' cat /data/f.txt 2>/dev/null | grep -q prova && exit 0
     sleep 0.2
   done; exit 1"

check "backup create --dry-run não escreve nada" ok "$BIN" backup create container "$BKC" --to "$BKDIR" --dry-run
check "backup create --dry-run mesmo não escreveu" ok bash -c "[[ -z \"\$(ls -A '$BKDIR')\" ]]"
check "backup create container" ok "$BIN" backup create container "$BKC" --to "$BKDIR"
check "o arquivo existe" ok bash -c "ls '$BKDIR'/container-$BKC-*.tar.gz >/dev/null"
check "o arquivo leva os dados do volume" ok bash -c \
  "tar tzf '$BKDIR'/container-$BKC-*.tar.gz | grep -q '^volumes/$BKV.tar.gz$'"
# A ENTRADA não são os DADOS. O check acima passa sobre um `volumes/x.tar.gz` de
# volume VAZIO, e um backup vazio restaura-se sem erro — o `restore` diz «volume
# restored» e o ficheiro não volta. Era o único sítio onde um arquivo oco passava
# por bom, e é o que deixava «os dados voltaram» a chumbar longe da causa.
check "…e o tarball do volume tem mesmo o ficheiro dentro" ok bash -c \
  "tar xzOf '$BKDIR'/container-$BKC-*.tar.gz 'volumes/$BKV.tar.gz' | tar tz | grep -q 'f.txt'"
check "e NÃO leva o rootfs (é derivável da imagem)" ok bash -c \
  "! tar tzf '$BKDIR'/container-$BKC-*.tar.gz | grep -q '^rootfs/'"

# Destruir para valer, e repor.
"$BIN" container exec "$BKC" rm -f /data/f.txt >/dev/null 2>&1
check "backup restore recusa-se com o container a correr" fail bash -c \
  "'$BIN' backup restore \$(ls '$BKDIR'/container-$BKC-*.tar.gz | head -1)"
check "backup restore --force pára, repõe e arranca" ok bash -c \
  "'$BIN' backup restore \$(ls '$BKDIR'/container-$BKC-*.tar.gz | head -1) --force"
# Espera por CONDIÇÃO e não por tempo. Era `sleep 1` a seguir a um
# `restore --force` que PÁRA e ARRANCA o container — e um segundo só chega
# quando a máquina está folgada. Medido a 2026-08-25: falhou numa corrida e
# passou na seguinte, sem nada ter mudado no motor. É a mesma armadilha que o
# AGENTS.md já regista a propósito da captura das imagens Proxmox — esperar por
# tempo na operação que mede o resultado.
# A janela era 30×0.2s = 6s, e 6s não chegaram (medido 2026-09-09, load ~18: o
# check chumbou, e a MESMA árvore e o MESMO binário deram FAIL=0 duas corridas
# depois a load 21). Esperar por condição não protege de nada se o limite for
# apertado: 100×0.2s = 20s. O limite existe para o teste terminar, não para o
# medir — se um dia estes 20s não bastarem, o problema é o restore e não a
# janela, e aí o vermelho é o certo.
check "os dados voltaram" ok bash -c \
  "for _ in \$(seq 100); do '$BIN' container exec '$BKC' cat /data/f.txt 2>/dev/null | grep -q prova && exit 0; sleep 0.2; done; exit 1"

# Classes de saída: «não existe» tem de ser distinguível de «rebentou».
check "backup create de inexistente devolve 4" 4 "$BIN" backup create container "nao-existe-$PFX" --to "$BKDIR"
check "backup restore de arquivo inexistente devolve 4" 4 "$BIN" backup restore "nao-existe-$PFX.tar.gz"
check "backup restore --kind trocado recusa" fail bash -c \
  "'$BIN' backup restore \$(ls '$BKDIR'/container-$BKC-*.tar.gz | head -1) --kind vm"
check "schedule --max-for-day que não divide o dia recusa" fail "$BIN" backup schedule container "$BKC" --to "$BKDIR" --max-for-day 5
check "schedule --cron @daily recusa (não se aproxima)" fail "$BIN" backup schedule container "$BKC" --to "$BKDIR" --cron "@daily"
check "schedule --cron com 4 campos recusa" fail "$BIN" backup schedule container "$BKC" --to "$BKDIR" --cron "0 2 * *"

# Os três verbos que a consolidação trouxe, e que antes NÃO existiam: sem eles a
# pergunta «que arquivos tenho» respondia-se com `ls`, e apagar um era `rm`.
check "backup ls mostra o arquivo" ok bash -c \
  "'$BIN' backup ls --from '$BKDIR' | grep -q '$BKC'"
check "backup ls --kind filtra" ok bash -c \
  "'$BIN' backup ls --from '$BKDIR' --kind container | grep -q '$BKC'"
check "backup ls --kind vm não traz um container" ok bash -c \
  "! '$BIN' backup ls --from '$BKDIR' --kind vm | grep -q '$BKC'"
check "backup inspect diz o kind e o nome" ok bash -c \
  "'$BIN' backup inspect \$(ls '$BKDIR'/container-$BKC-*.tar.gz | head -1) | grep -q '$BKC'"
check "backup inspect nomeia os volumes que leva" ok bash -c \
  "'$BIN' backup inspect \$(ls '$BKDIR'/container-$BKC-*.tar.gz | head -1) | grep -q '$BKV'"
check "backup inspect de inexistente devolve 4" 4 "$BIN" backup inspect "nao-existe-$PFX.tar.gz"

# A guarda que impede o `remove` de apagar um ficheiro alheio. Verifica-se pelo
# FICHEIRO e não pelo código de saída: um `remove` que recusa e apaga na mesma
# devolveria não-zero e teria destruído os dados à mesma.
echo lixo | gzip > "$BKDIR/alheio.tar.gz"
check "backup remove recusa um .tar.gz que não escrevemos" fail \
  "$BIN" backup remove alheio.tar.gz --from "$BKDIR"
check "e o ficheiro alheio CONTINUA lá" ok bash -c "[[ -f '$BKDIR/alheio.tar.gz' ]]"
check "backup ls conta o alheio como saltado" ok bash -c \
  "'$BIN' backup ls --from '$BKDIR' | grep -q 'skipped\|saltado'"
rm -f "$BKDIR/alheio.tar.gz"

check "backup remove apaga o nosso" ok bash -c \
  "'$BIN' backup remove \$(basename \$(ls '$BKDIR'/container-$BKC-*.tar.gz | head -1)) --from '$BKDIR'"
check "e o arquivo desapareceu" ok bash -c \
  "! ls '$BKDIR'/container-$BKC-*.tar.gz >/dev/null 2>&1"

# O corte limpo: a forma antiga falha ALTO, nunca em silêncio (precedente v0.30.0).
check "o \`restore\` de raiz deixou de existir" 2 "$BIN" restore container x
check "o \`backup <kind>\` sem verbo deixou de existir" 2 "$BIN" backup container "$BKC"

# E o âmbito de nó NÃO foi dobrado aqui: é outro objecto (o state root do nó),
# e o ADR-0020 chegou a classificá-lo como uma segunda porta para este grupo —
# corrigido depois. Só a PALAVRA colidia (não o âmbito); desde o Sprint 4 da
# reestruturação da CLI, o verbo de nó chama-se `system snapshot`, e o antigo
# `system backup`/`system restore` falha com "unrecognized subcommand".
check "system backup deixou de existir (corte limpo)" 2 "$BIN" system backup --help
check "system restore deixou de existir (corte limpo)" 2 "$BIN" system restore --help
check "system snapshot create continua a existir, separado" ok "$BIN" system snapshot create --help
check "system snapshot restore continua a existir, separado" ok "$BIN" system snapshot restore --help

"$BIN" container rm -f "$BKC" >/dev/null 2>&1
"$BIN" volume rm -f "$BKV" >/dev/null 2>&1

section "serve — arrancar, sondar, matar, e não deixar restos"

# O grupo `serve` tinha ZERO execuções. São SERVIDORES, por isso precisa de um
# padrão próprio, e o padrão é a parte que interessa reter:
#
#   1. arrancar DETACHED com a saída para ficheiro (nunca para um pipe que a
#      bateria leia — um servidor longevo segura esse fd e o `read` do shell
#      nunca vê EOF; foi assim que o pin pendurou uma corrida 31 minutos);
#   2. esperar por CONDIÇÃO (o socket existir), nunca por tempo;
#   3. sondar a sério — um socket que aceita não é um servidor que responde;
#   4. matar e confirmar que morreu.
#
# Sockets em /tmp e não em $OUT: `sun_path` do AF_UNIX são 108 bytes.
SRVLOG="$OUT/serve-$PFX.log"

# --- fail-closed: o tecto de capabilities recusa ANTES de qualquer bind -------
# Vale mais que o caminho feliz: um tecto que caísse em silêncio para "ilimitado"
# por causa de um typo é exactamente a falha que ele existe para evitar.
CAPSOCK="/tmp/dlx-cap-$PFX.sock"; rm -f "$CAPSOCK"
check "serve cri recusa uma capability desconhecida" fail \
  "$BIN" serve cri --addr "unix://$CAPSOCK" --cap-ceiling "NAO_EXISTE_CAP"
check "e NÃO chegou a criar o socket" ok bash -c "[ ! -S '$CAPSOCK' ]"
check "e a recusa NOMEIA a capability" ok bash -c \
  "'$BIN' serve cri --addr 'unix://$CAPSOCK' --cap-ceiling NAO_EXISTE_CAP 2>&1 | grep -q NAO_EXISTE_CAP"
check "serve cri recusa um modo de tecto desconhecido" fail \
  "$BIN" serve cri --addr "unix://$CAPSOCK" --cap-ceiling-mode xyz
check "e também aqui não criou socket" ok bash -c "[ ! -S '$CAPSOCK' ]"

# --- o helper: sobe, espera pelo socket, devolve o pid -----------------------
e2e_serve_up() {  # $1=subcomando  $2=socket  → ecoa o pid, ou vazio
  local sub="$1" sock="$2"
  rm -f "$sock"
  setsid "$BIN" serve "$sub" --addr "unix://$sock" >>"$SRVLOG" 2>&1 &
  local i
  for i in $(seq 1 60); do [ -S "$sock" ] && break; sleep 0.2; done
  # Pelo DONO do socket, não pelo cmdline: `serve cri` executa o `delonix-cri`, e
  # o cmdline passa a ser o dele.
  ss -xlpnH 2>/dev/null | grep -F "$sock" | grep -o 'pid=[0-9]*' | head -1 | cut -d= -f2
}

for spec in "api:/v1/dash" "docker-api:/_ping"; do
  sub="${spec%%:*}"; path="${spec#*:}"
  SOCK="/tmp/dlx-srv-$sub-$PFX.sock"
  PID="$(e2e_serve_up "$sub" "$SOCK")"
  check "serve $sub cria o socket" ok bash -c "[ -S '$SOCK' ]"
  check "serve $sub continua vivo depois de ligar" ok bash -c "[ -n '$PID' ] && kill -0 '$PID'"
  # Um socket que ACEITA não é um servidor que responde — daí o HTTP real.
  check "serve $sub responde 200 em $path" ok bash -c \
    "[ \"\$(curl -s -o /dev/null -w '%{http_code}' --unix-socket '$SOCK' 'http://localhost$path' --max-time 30)\" = 200 ]"
  [ -n "$PID" ] && kill "$PID" 2>/dev/null
  for i in $(seq 1 40); do kill -0 "$PID" 2>/dev/null || break; sleep 0.2; done
  check "serve $sub morre com SIGTERM" ok bash -c "! kill -0 '$PID' 2>/dev/null"
  rm -f "$SOCK"
done

# `POST /containers/{id}/wait` tem de devolver o código REAL. Um container criado
# por esta API corria sem supervisor (resquício de quando o arranque corria no
# próprio servidor multi-thread), ninguém era o pai do processo, e o `wait`
# respondia «exit code was not captured» — `exit 7` ficava `Exited (unknown)`.
DSOCK="/tmp/dlx-srv-dwait-$PFX.sock"
DPID="$(e2e_serve_up docker-api "$DSOCK")"
DID=$(curl -s --unix-socket "$DSOCK" --max-time 120 -X POST -H 'Content-Type: application/json' \
  -d "{\"Image\":\"$IMG\",\"Cmd\":[\"sh\",\"-c\",\"sleep 1; exit 7\"],\"HostConfig\":{\"NetworkMode\":\"none\"}}" \
  "http://localhost/containers/create?name=dwait-$PFX" | python3 -c "import sys,json; print(json.load(sys.stdin).get('Id',''))" 2>/dev/null)
check "docker-api: POST /wait devolve o código real (7)" ok bash -c \
  "[ -n '$DID' ] && curl -s --unix-socket '$DSOCK' --max-time 60 -X POST 'http://localhost/containers/$DID/wait' | grep -q '\"StatusCode\":7'"
curl -s --unix-socket "$DSOCK" --max-time 30 -X DELETE "http://localhost/containers/$DID?force=1" >/dev/null 2>&1
# `HostConfig.RestartPolicy` era RECUSADO com o argumento do fork num servidor
# multi-thread — que deixou de valer quando o arranque passou ao re-exec
# `__apirun`. Agora é servido: `on-failure` com `MaximumRetryCount: 2` reinicia
# duas vezes, e um nome de política errado continua a ser recusado.
RID=$(curl -s --unix-socket "$DSOCK" --max-time 120 -X POST -H 'Content-Type: application/json' \
  -d "{\"Image\":\"$IMG\",\"Cmd\":[\"sh\",\"-c\",\"exit 3\"],\"HostConfig\":{\"NetworkMode\":\"none\",\"RestartPolicy\":{\"Name\":\"on-failure\",\"MaximumRetryCount\":2}}}" \
  "http://localhost/containers/create?name=drest-$PFX" | python3 -c "import sys,json; print(json.load(sys.stdin).get('Id',''))" 2>/dev/null)
check "docker-api: RestartPolicy on-failure é aceite" ok bash -c "[ -n '$RID' ]"
check "docker-api: MaximumRetryCount 2 dá RESTARTS 2" ok bash -c \
  "for _ in \$(seq 1 30); do '$BIN' container ls -a -o json | python3 -c \"import json,sys; sys.exit(0 if any(c.get('names',c.get('name'))=='drest-$PFX' and c.get('restarts')==2 for c in json.load(sys.stdin)) else 1)\" && exit 0; sleep 1; done; exit 1"
check "docker-api: uma política desconhecida é recusada" ok bash -c \
  "curl -s --unix-socket '$DSOCK' --max-time 30 -X POST -H 'Content-Type: application/json' -d '{\"Image\":\"$IMG\",\"Cmd\":[\"true\"],\"HostConfig\":{\"RestartPolicy\":{\"Name\":\"alwayz\"}}}' 'http://localhost/containers/create?name=dbad-$PFX' | grep -q 'not a restart policy'"
# O estado HTTP sai do TIPO do erro, os que o Docker usa. Respondia 500 a tudo o
# que não dissesse «not found» — um nome em uso e um argumento inválido liam-se
# como «o servidor avariou».
check "docker-api: um nome já em uso responde 409" ok bash -c \
  "[ \"\$(curl -s -o /dev/null -w '%{http_code}' --unix-socket '$DSOCK' --max-time 60 -X POST -H 'Content-Type: application/json' -d '{\"Image\":\"$IMG\",\"Cmd\":[\"true\"],\"HostConfig\":{\"NetworkMode\":\"none\"}}' 'http://localhost/containers/create?name=drest-$PFX')\" = 409 ]"
check "docker-api: um argumento inválido responde 400" ok bash -c \
  "[ \"\$(curl -s -o /dev/null -w '%{http_code}' --unix-socket '$DSOCK' --max-time 30 -X POST -H 'Content-Type: application/json' -d '{\"Image\":\"$IMG\",\"Cmd\":[\"true\"],\"HostConfig\":{\"RestartPolicy\":{\"Name\":\"alwayz\"}}}' 'http://localhost/containers/create?name=dbad2-$PFX')\" = 400 ]"
# Uma imagem que não existe é «não existe» (404 / classe 4), e não uma avaria do
# registo. O Docker Hub responde 401 a um repositório inexistente e o ghcr 403 no
# token — os dois saíam como `registry error`, 500 e código 1.
check "image pull de um repositório inexistente sai com a classe 4" 4 \
  "$BIN" image pull "nao-existe-$PFX-777:1"
check "docker-api: create com uma imagem inexistente responde 404" ok bash -c \
  "[ \"\$(curl -s -o /dev/null -w '%{http_code}' --unix-socket '$DSOCK' --max-time 120 -X POST -H 'Content-Type: application/json' -d '{\"Image\":\"nao-existe-$PFX-777:1\",\"Cmd\":[\"true\"],\"HostConfig\":{\"NetworkMode\":\"none\"}}' 'http://localhost/containers/create?name=dnoimg-$PFX')\" = 404 ]"
check "container run com um nome em uso sai com a classe de conflito" 5 \
  "$BIN" container run -d --net none --name "drest-$PFX" "$IMG" true
curl -s --unix-socket "$DSOCK" --max-time 30 -X DELETE "http://localhost/containers/$RID?force=1" >/dev/null 2>&1
[ -n "$DPID" ] && kill "$DPID" 2>/dev/null
for i in $(seq 1 40); do kill -0 "$DPID" 2>/dev/null || break; sleep 0.2; done
rm -f "$DSOCK"

# O CRI fala gRPC, não HTTP — a sonda honesta é o socket mais o processo vivo,
# e é isso que se afirma, em vez de fingir um pedido que não sabemos fazer aqui.
CRISOCK="/tmp/dlx-srv-cri-$PFX.sock"
CRIPID="$(e2e_serve_up cri "$CRISOCK")"
check "serve cri cria o socket" ok bash -c "[ -S '$CRISOCK' ]"
check "serve cri continua vivo (gRPC: não se sonda por HTTP aqui)" ok bash -c \
  "[ -n '$CRIPID' ] && kill -0 '$CRIPID'"
[ -n "$CRIPID" ] && kill "$CRIPID" 2>/dev/null
for i in $(seq 1 40); do kill -0 "$CRIPID" 2>/dev/null || break; sleep 0.2; done
check "serve cri morre com SIGTERM" ok bash -c "! kill -0 '$CRIPID' 2>/dev/null"
rm -f "$CRISOCK"

# `delonix serve cri` executa o binário próprio do CRI (ADR-0040 D2.4 emendado): o
# utilizador só conhece `delonix`, e o servidor não vive dentro dele. O `exec`
# mantém o pid, por isso é o `delonix-cri` que se vê ao fim do socket.
CRISOCK2="/tmp/dlx-srv-cri2-$PFX.sock"
CRIPID2="$(e2e_serve_up cri "$CRISOCK2")"
check "serve cri corre o executável delonix-cri" ok bash -c \
  "[ -n '$CRIPID2' ] && [ \"\$(basename \"\$(readlink /proc/$CRIPID2/exe)\")\" = delonix-cri ]"
[ -n "$CRIPID2" ] && kill "$CRIPID2" 2>/dev/null
for i in $(seq 1 40); do kill -0 "$CRIPID2" 2>/dev/null || break; sleep 0.2; done
rm -f "$CRISOCK2"
# Sem o binário ao lado nem no PATH: recusa com a classe «indisponível» e diz
# como instalar, em vez de um `No such file or directory` sem sujeito.
LONE="$OUT/lone-$PFX"; mkdir -p "$LONE"; cp "$BIN" "$LONE/delonix"
check "serve cri sem o delonix-cri instalado sai com 69" 69 \
  env PATH=/usr/bin:/bin "$LONE/delonix" serve cri --addr "unix:///tmp/dlx-lone-$PFX.sock"
check "e diz como instalar" ok bash -c \
  "env PATH=/usr/bin:/bin '$LONE/delonix' serve cri --addr 'unix:///tmp/dlx-lone-$PFX.sock' 2>&1 | grep -q 'with-cri'"
rm -rf "$LONE"

# `delonix mcp` executa o `delonix-mcp` (P3l). As mutações do MCP correm a CLI de
# volta — e tem de ser o `delonix`, nunca o próprio `delonix-mcp`, que re-correria o
# servidor em vez do comando.
check "mcp capabilities responde pelo delonix-mcp" ok bash -c \
  "'$BIN' mcp capabilities | python3 -c 'import json,sys; sys.exit(0 if any(t[\"tool\"]==\"logs.query\" for t in json.load(sys.stdin)) else 1)'"
check "mcp doctor resolve a CLI delonix e não o delonix-mcp" ok bash -c \
  "'$BIN' mcp doctor 2>&1 | grep 'runtime_binary_resolvable' | grep -q '/delonix\$'"
LONE="$OUT/lone-mcp-$PFX"; mkdir -p "$LONE"; cp "$BIN" "$LONE/delonix"
check "mcp sem o delonix-mcp instalado sai com 69" 69 \
  env PATH=/usr/bin:/bin "$LONE/delonix" mcp capabilities
rm -rf "$LONE"

# `delonix serve api` executa o `delonix-mgmt` (P3m), e as rotas que correm a CLI de
# volta chegam ao `delonix`, não ao próprio servidor.
APISOCK="/tmp/dlx-srv-api2-$PFX.sock"
APIPID="$(e2e_serve_up api "$APISOCK")"
check "serve api corre o executável delonix-mgmt" ok bash -c \
  "[ -n '$APIPID' ] && [ \"\$(basename \"\$(readlink /proc/$APIPID/exe)\")\" = delonix-mgmt ]"
[ -n "$APIPID" ] && kill "$APIPID" 2>/dev/null
for i in $(seq 1 40); do kill -0 "$APIPID" 2>/dev/null || break; sleep 0.2; done
rm -f "$APISOCK"
LONE="$OUT/lone-api-$PFX"; mkdir -p "$LONE"; cp "$BIN" "$LONE/delonix"
check "serve api sem o delonix-mgmt instalado sai com 69" 69 \
  env PATH=/usr/bin:/bin "$LONE/delonix" serve api --addr "unix:///tmp/dlx-lone-api-$PFX.sock"
rm -rf "$LONE"

check "nenhum servidor desta corrida ficou para trás" ok bash -c \
  "! pgrep -f '(serve (cri|api|docker-api)|delonix-cri|delonix-mgmt) --addr unix:///tmp/dlx-srv-.*$PFX' >/dev/null"

section "compose — o que é recusado, e se a recusa dispara"

# O `compose` tinha ZERO execuções. Metade do valor aqui não é o caminho feliz —
# é confirmar que as lacunas DECLARADAS no v1 falham ALTO. A armadilha que esta
# base de código pagou mais vezes é a opção aceite e descartada em silêncio
# (`--security-opt seccomp=`, `-v …:z`, `--network-alias`, `--subnet`), e o
# `compose.rs` declara sete recusas que nada exercitava.
# NOTA: as flags vão DEPOIS do subcomando (`compose config -f X`), não antes.
# A primeira versão desta secção pôs `-f` antes e chumbou 10 checks com
# "unexpected argument '-f'" — verificar UMA invocação à mão antes de escrever
# a secção teria custado trinta segundos.
CWORK="$OUT/compose-$PFX"; mkdir -p "$CWORK"

cat >"$CWORK/docker-compose.yml" <<YAML
services:
  web-$PFX:
    image: alpine:3.19
    command: ["sleep", "600"]
    environment:
      GREETING: ola
    working_dir: /tmp
YAML

check "compose config aceita um ficheiro válido" ok \
  "$BIN" compose config -f "$CWORK/docker-compose.yml" -p "cp$PFX"
check "compose config resolve o serviço" ok bash -c \
  "'$BIN' compose config -f '$CWORK/docker-compose.yml' -p 'cp$PFX' | grep -q 'web-$PFX'"
check "compose -f inexistente falha" fail \
  "$BIN" compose config -f "$CWORK/naoexiste.yml" -p "cp$PFX"

# --- as recusas do v1: cada uma tem de NOMEAR o campo ------------------------
cat >"$CWORK/replicas.yml" <<YAML
services:
  s-$PFX:
    image: alpine:3.19
    deploy:
      replicas: 3
YAML
# Estes dois checks nasceram quando o motor RECUSAVA `deploy.replicas` e
# esperavam a recusa. Hoje o campo está implementado: `replicas: 3` resolve para
# TRÊS containers, com o sufixo `-2`/`-3` no nome. Um check que exige uma recusa
# que já não acontece não é «o produto regrediu» — é o teste a ficar velho, e
# apagá-lo perdia a única prova de que a implementação faz o que diz.
check "compose deploy.replicas: 3 resolve para 3 containers" ok bash -c \
  "[ \"\$('$BIN' compose config -f '$CWORK/replicas.yml' -p 'cp$PFX' | grep -c '^  container:')\" -eq 3 ]"
# E os três têm NOMES distintos: três linhas com o mesmo nome seriam um só
# container contado três vezes, que é o defeito que esta grafia convida.
check "…e os 3 têm nomes distintos" ok bash -c \
  "[ \"\$('$BIN' compose config -f '$CWORK/replicas.yml' -p 'cp$PFX' | awk '/^  container:/{print \$2}' | sort -u | wc -l)\" -eq 3 ]"

cat >"$CWORK/extends.yml" <<YAML
services:
  s-$PFX:
    image: alpine:3.19
    extends:
      service: outro
YAML
check "compose recusa extends:" fail \
  "$BIN" compose config -f "$CWORK/extends.yml" -p "cp$PFX"
check "e a recusa NOMEIA o extends" ok bash -c \
  "'$BIN' compose config -f '$CWORK/extends.yml' -p 'cp$PFX' 2>&1 | grep -qi 'extends'"

cat >"$CWORK/profiles.yml" <<YAML
services:
  s-$PFX:
    image: alpine:3.19
    profiles: ["dev"]
YAML
# Idem para `profiles:`: já não é recusado, é HONRADO. Um serviço com profile
# fica FORA do projecto resolvido até alguém pedir `--profile`, e é esse par que
# vale medir — a metade «não aparece» sozinha passaria com o campo ignorado e o
# serviço a faltar por outra razão qualquer.
check "compose profiles: sem --profile o serviço fica FORA" ok bash -c \
  "! '$BIN' compose config -f '$CWORK/profiles.yml' -p 'cp$PFX' | grep -q 's-$PFX'"
check "…e com --profile dev entra" ok bash -c \
  "'$BIN' compose config -f '$CWORK/profiles.yml' -p 'cp$PFX' --profile dev | grep -q 's-$PFX'"

# A porta com IP de host tem HISTÓRIA, e a primeira versão deste check estava
# errada: o `compose.rs` chegou a DESCARTAR o IP em silêncio (publicando em
# todas as interfaces o oposto do que o ficheiro pedia), foi corrigido para
# RECUSAR, e depois o motor ganhou suporte real à forma `[ip:]host:cont`
# (`parse_publish_addr`, 2026-07-27) — logo já não recusa, honra. Escrevi o
# check contra a fase do meio e ele chumbou; o que vale medir não é a recusa,
# é o ENDEREÇO em que a porta fica ligada. Um IP descartado voltaria a publicar
# em 0.0.0.0 e este check apanha-o.
cat >"$CWORK/hostip.yml" <<YAML
services:
  s-$PFX:
    image: alpine:3.19
    command: ["sleep", "600"]
    ports:
      - "127.0.0.1:19099:80"
YAML
check "compose aceita porta com IP de host" ok \
  "$BIN" compose up -d -f "$CWORK/hostip.yml" -p "cp$PFX"
check "e o registo guarda o IP pedido, não 0.0.0.0" ok bash -c \
  "'$BIN' container port 'cp$PFX-s-$PFX' | grep -q '127\.0\.0\.1:19099'"
check "e o bind REAL do host é loopback (não todas as interfaces)" ok bash -c \
  "ss -tlnH | grep -q '127\.0\.0\.1:19099'"
check "e nada ficou ligado em 0.0.0.0:19099" ok bash -c \
  "! ss -tlnH | grep -q '0\.0\.0\.0:19099'"
"$BIN" compose down -f "$CWORK/hostip.yml" -p "cp$PFX" >/dev/null 2>&1

# --- ciclo real -------------------------------------------------------------
check "compose up -d" ok \
  "$BIN" compose up -d -f "$CWORK/docker-compose.yml" -p "cp$PFX"
check "compose ps lista o serviço" ok bash -c \
  "'$BIN' compose ps -f '$CWORK/docker-compose.yml' -p 'cp$PFX' | grep -q 'web-$PFX'"
check "o working_dir do compose foi aplicado" ok bash -c \
  "'$BIN' container exec cp$PFX-web-$PFX pwd 2>/dev/null | grep -qx /tmp || \
   '$BIN' container exec web-$PFX pwd 2>/dev/null | grep -qx /tmp"
check "compose logs responde" ok \
  "$BIN" compose logs -f "$CWORK/docker-compose.yml" -p "cp$PFX"
check "compose down limpa" ok \
  "$BIN" compose down -f "$CWORK/docker-compose.yml" -p "cp$PFX"
check "e o serviço deixou de aparecer" ok bash -c \
  "! '$BIN' compose ps -f '$CWORK/docker-compose.yml' -p 'cp$PFX' 2>/dev/null | grep -q 'Up'"

section "net — a plumbing que nunca era executada"

# Porque esta secção existe: o grupo `net` tem 43 folhas em 6 subgrupos e a
# bateria executava ZERO delas. Foi onde viveram todos os defeitos da série de
# 2026-08-15 (o reaper de slirp a ceifar processos de outras ferramentas, o
# `kill_pidfile` sem identidade, o `runtime_dir` partilhado entre roots, o lock
# em falta, o pin a segurar o stderr do chamador) — e nenhum apareceu aqui: os
# dois hangs só se manifestaram por `container`/`vm` arrastarem a rede por baixo.
#
# Precisa dos DOIS roots isolados. Sem eles isto mexe na infra real da máquina.
if [[ -z "${DELONIX_ROOT:-}" || -z "${DELONIX_NET_RUNTIME_DIR:-}" ]]; then
  skip "net: ciclo do netns" "exige DELONIX_ROOT E DELONIX_NET_RUNTIME_DIR (ver cabeçalho)"
else
  # --- ciclo de vida da infra -------------------------------------------------
  check "net netns status responde parado" ok "$BIN" net netns status
  check "net netns up" ok "$BIN" net netns up
  check "net netns status diz UP" ok bash -c \
    "'$BIN' net netns status | grep -qi 'ingress UP'"
  # Idempotente: subir duas vezes não pode reconstruir nada.
  check "net netns up é idempotente" ok "$BIN" net netns up

  # O pin NÃO pode segurar o stdout/stderr de quem o arrancou. Antes de
  # 2026-08-15 segurava, e um `$(...)` sobre qualquer comando que levantasse a
  # infra bloqueava para sempre. `timeout` é o teste: se o pipe ficar preso, o
  # comando nunca devolve.
  check "um comando que CAPTURA a saída não fica preso no pin" ok bash -c \
    "out=\$(timeout 20 '$BIN' net netns status 2>&1); [ -n \"\$out\" ]"

  # --- ingress / egress: as regras por-container ------------------------------
  check "net ingress ls" ok "$BIN" net ingress ls

# --- as leituras do `net` passam a ser legíveis por PROGRAMA (C-3) ---------
# O grupo inteiro era tabela-e-só: toda a observabilidade de rede deste motor
# obrigava um script a parsear colunas alinhadas E TRADUZIDAS (`--l18n=pt` muda
# os cabeçalhos). Estes checks passam a saída por um parser de JSON a sério, e
# não por `grep` — um `grep` passa numa tabela com aspas.
for leitura in "net ingress ls" "net egress ls"; do
  check "$leitura -o json é JSON a sério" ok bash -c \
    "'$BIN' $leitura -o json | python3 -c 'import json,sys; json.load(sys.stdin)'"
done
# O `governed` existe para separar «não governado» de «aberto» — a tabela
# dobra os dois numa frase (`n/a (host net)` contra `allow (default)`), e era
# essa a razão de ADR-0005 aqui.
check "ingress ls json separa governado de aberto" ok bash -c \
  "'$BIN' net ingress ls -o json | python3 -c \"import json,sys; d=json.load(sys.stdin); sys.exit(0 if all('governed' in r for r in d) else 1)\""
  check "net egress ls" ok "$BIN" net egress ls
  check "net ingress de um container inexistente diz 4" 4 \
    "$BIN" net ingress allow naoexiste-$PFX 80
  check "net egress de um container inexistente diz 4" 4 \
    "$BIN" net egress allow naoexiste-$PFX 80

  # --- os outros verbos respondem, e a classe de erro é a certa ---------------
  # As LISTAGENS de HTTPRoute e Gateway não vivem no `net`: o `net httproute`
  # tem apply/rm e o `net tunnel` tem expose/apply. Quem lista é o verbo
  # genérico — `get httproutes` e `get gateways`.
  check "get httproutes" ok "$BIN" get httproutes
  check "get gateways" ok "$BIN" get gateways
  # Estes dois checks nasceram no #255 a exigir que `-o json` fosse RECUSADO com
  # razão escrita — que era o comportamento medido na v3.0.0. O #259 descobriu
  # porque: o `NO_JSON_YET` do `verbs.rs` listava-os há muito, e o
  # `tunnel::cmd_ls`/`httproute::cmd_ls` já tinham braço de JSON completo. A
  # recusa era a lista a estar velha, não uma decisão. Hoje respondem JSON.
  #
  # O portão apanhou a mudança na primeira corrida depois do #259 fundir, com os
  # dois a chumbarem por rc=0 onde esperavam 1 — que é exactamente o serviço que
  # ele passou a prestar. Passam a medir o que é verdade, e por um PARSER de JSON
  # a sério e não por `grep`: um `grep` passa numa tabela com aspas.
  for _k in httproutes gateways; do
    check "get $_k -o json é JSON a sério" ok bash -c \
      "'$BIN' get $_k -o json | python3 -c 'import json,sys; json.load(sys.stdin)'"
  done
  # E o Kind que CONTINUA sem JSON é recusado, não fingido — a lista encolheu,
  # não desapareceu, e recusado-com-razão e em-falta são estados diferentes.
  check "get kubernetesclusters -o json continua recusado, com razão" 1 \
    "$BIN" get kubernetesclusters -o json
  check "…e a recusa diz que a listagem é table-only" ok bash -c \
    "'$BIN' get kubernetesclusters -o json 2>&1 | grep -q 'table-only'"
  check "net flow --help" ok "$BIN" net flow --help
  # `net boot` dobrou-se em `system boot` no B2 (#151), com `namespace`.
  check "system boot status" ok "$BIN" system boot status
  # O grupo tem `status`, não `ls`. Fica fixado: a primeira versão deste check
  # assumiu `ls` (o verbo do resto da CLI) e chumbou com rc=2 — se algum dia o
  # `ls` for acrescentado, é uma escolha e não um acidente.
  check "system boot ls NÃO existe (é status)" 2 "$BIN" system boot ls

  # `system boot enable/disable` escreve units systemd em ~/.config/systemd/user,
  # que o DELONIX_ROOT NÃO redirecciona — num host com produção isso mexe fora
  # do isolamento. Fica declarado por cobrir, nunca corrido às escondidas.
  skip "system boot enable/disable" "escreve units em ~/.config/systemd/user, fora do DELONIX_ROOT"

  # --- reiniciar UM membro de um pod: o fim do antigo não apaga o novo ---------
  # Medido a 2026-09-17 na `main` (6209d59d/4fc211a6): `stop -t 0` + `start` de um
  # membro deixava-o `Crashed`, `pid: null`, sem `crash_reason` e com o log vazio
  # em 7 de 10 corridas — e o processo novo vivo, sem nada no store a apontar
  # para ele. Duas metades, e a bateria mede as duas:
  #   * o `stop` devolvia logo a seguir ao SIGKILL, com o processo ainda a sair
  #     (o `nc` em `D`, `wb_wait_for_completion`, 1,5–6,8 s neste host);
  #   * o supervisor antigo, ao acabar o `waitpid`, gravava `Crashed` por cima do
  #     registo que o `start` já tinha passado à incarnação nova.
  # A primeira é determinista (o pid antigo tem de ter saído quando o `stop`
  # devolve); a segunda depende do disco, por isso vai em ciclos, e a prova de
  # vida não é o `status` — é o pid existir e o membro responder pela netns do pod.
  PRY="$OUT/podrestart-$PFX.yaml"
  cat >"$PRY" <<YAML
apiVersion: compute.delonix.io/v1alpha1
kind: Pod
metadata:
  name: pr$PFX
spec:
  containers:
    - name: srv
      image: $IMG
      command: ["sh", "-c", "while true; do printf pong | nc -l -p 7070; done"]
    - name: cli
      image: $IMG
      command: ["sleep", "600"]
YAML
  # As duas provas num script à parte: aspas de três níveis dentro de `bash -c`
  # são onde um check destes passa a verde por não correr o que diz.
  PRS="$OUT/podrestart-$PFX.sh"
  cat >"$PRS" <<'SH'
#!/usr/bin/env bash
# uso: podrestart.sh <bin> <pod> stop-espera|ciclos
BIN=$1; POD=$2
pid_of() { "$BIN" container inspect "$1" | python3 -c \
  'import json,sys; d=json.load(sys.stdin); d=d[0] if isinstance(d,list) else d; print(d.get("pid") or "")'; }
case $3 in
  stop-espera)
    old=$(pid_of "$POD-srv")
    [ -n "$old" ] || { echo "o membro não tinha pid antes do stop"; exit 1; }
    "$BIN" container stop -t 0 "$POD-srv" >/dev/null || exit 1
    st=$(awk '{print $3}' "/proc/$old/stat" 2>/dev/null)
    rc=0
    [ -z "$st" ] || [ "$st" = Z ] || { echo "pid $old ainda existe ($st) quando o stop devolveu"; rc=1; }
    "$BIN" container start "$POD-srv" >/dev/null
    exit $rc ;;
  ciclos)
    for i in 1 2 3 4 5; do
      "$BIN" container stop -t 0 "$POD-srv" >/dev/null || { echo "ciclo $i: stop falhou"; exit 1; }
      "$BIN" container start "$POD-srv" >/dev/null || { echo "ciclo $i: start falhou"; exit 1; }
      sleep 3
      st=$("$BIN" container inspect "$POD-srv" | grep -m1 '"status"')
      pid=$(pid_of "$POD-srv")
      { [ -n "$pid" ] && [ -e "/proc/$pid" ]; } || { echo "ciclo $i: $st pid=${pid:-null}"; exit 1; }
      r=
      for _ in 1 2 3 4 5; do
        r=$(timeout 5 "$BIN" pod exec "$POD" --container cli sh -c 'nc 127.0.0.1 7070 </dev/null' 2>/dev/null)
        [ "$r" = pong ] && break
        sleep 1
      done
      [ "$r" = pong ] || { echo "ciclo $i: $st pid=$pid, mas o membro não respondeu"; exit 1; }
    done ;;
esac
SH
  # A infra está de pé aqui (secção acima): um apply que falha é um FAIL, não um
  # skip — um skip calaria a regressão que este bloco existe para apanhar.
  check "pod: stack apply de um pod de dois membros" ok "$BIN" stack apply -f "$PRY"
  if "$BIN" container inspect "pr$PFX-srv" >/dev/null 2>&1; then
    check "pod: stop -t 0 de um membro só devolve com o processo fora" ok \
      bash "$PRS" "$BIN" "pr$PFX" stop-espera
    check "pod: 5× stop -t 0 + start de um membro — vivo e a responder" ok \
      bash "$PRS" "$BIN" "pr$PFX" ciclos
    "$BIN" delete pod "pr$PFX" -f >/dev/null 2>&1 || true
  fi

  # --- e a infra desce sem deixar restos --------------------------------------
  check "net netns down" ok "$BIN" net netns down
  check "net netns status diz parado outra vez" ok bash -c \
    "! '$BIN' net netns status | grep -qi 'ingress UP'"
  check "o socket de controlo não ficou para trás" ok bash -c \
    "[ ! -S \"\$DELONIX_NET_RUNTIME_DIR/control.sock\" ]"
  # down duas vezes é idempotente (é o comando de recuperação de um host).
  check "net netns down é idempotente" ok "$BIN" net netns down
fi

section "api-resources: o registo que os outros verbos leem"
########################################
# É o primeiro comando da árvore-alvo a aterrar, e o único da CLI-2 que não
# depende da reestruturação dos Kinds: lista o que houver no registo, e as
# LINHAS mudam com a reestruturação sem o mecanismo mudar.
#
# Não há segunda tabela por baixo — a listagem deriva do mesmo `cmd::kinds`
# que o parser, o schema, a completação e o reconciliador leem. Por isso o que
# se verifica aqui não é o conteúdo (isso é teste unitário, e derivado não pode
# divergir): é que o comando existe, responde nos dois formatos, e que o JSON
# cumpre o contrato que a automação lê.
check "api-resources responde" ok "$BIN" api-resources
check "api-resources -o json é um array não vazio" ok bash -c \
  "'$BIN' api-resources -o json | python3 -c 'import json,sys; v=json.load(sys.stdin); assert isinstance(v, list) and v'"
check "cada linha traz as colunas que a automação lê" ok bash -c \
  "'$BIN' api-resources -o json | python3 -c \"
import json,sys
for r in json.load(sys.stdin):
    for k in ('name','shortNames','apiVersion','kind','namespaced','domain','form'):
        assert k in r, (r.get('kind'), k)
    assert isinstance(r['shortNames'], list)   # array vazio continua array
    assert isinstance(r['namespaced'], bool)   # nunca a string 'true'
\""
# A mesma regra do resto do `-o json`: o que a automação lê não muda de língua.
check "api-resources -o json é idêntico em EN e PT" ok bash -c \
  "diff <('$BIN' api-resources -o json) <('$BIN' --l18n=pt api-resources -o json)"
# E o registo tem de bater com o RESOLVEDOR: um plural listado que o `explain`
# não aceitasse seria a tabela a documentar um nome que não funciona. As duas
# respostas legítimas são explicar, ou recusar com «no typed schema» — que é uma
# propriedade do Kind (o `Storage` é reescrito para `Volume`), não do nome.
check "todo o plural listado resolve no explain" ok \
  python3 - "$BIN" <<'PYNAMES'
import json, subprocess, sys

BIN = sys.argv[1]
rows = json.loads(subprocess.run([BIN, "api-resources", "-o", "json"],
                                 capture_output=True, text=True, check=True).stdout)
assert rows, "api-resources devolveu vazio"
for r in rows:
    for name in (r["name"], r["kind"], *r["shortNames"]):
        p = subprocess.run([BIN, "explain", name], capture_output=True, text=True)
        if p.returncode == 0:
            continue
        if "no typed schema" in p.stdout + p.stderr:
            continue
        sys.exit(f"{r['kind']}: `explain {name}` nao resolve — {p.stderr.strip()[:120]}")
PYNAMES

section "cancelamento: um terminal em modo raw não é nosso para deixar partido"
########################################
# BUG MEDIDO a 2026-08-26, antes de haver correcção: um `SIGTERM` a um
# `container exec -it` deixava a shell de quem chamou com `ECHO` e `ICANON`
# desligados — sem eco e sem edição de linha, até se escrever `reset` às cegas.
#
# A causa não é descuido: o `restore_mode` corre em todas as saídas NORMAIS,
# incluindo a de erro (é por isso que o `?` do `exec` está depois dele). Um
# sinal é que não corre código Rust nenhum — nem destrutores, nem unwinding.
# Acontece com qualquer morte por sinal: um `kill`, um timeout de CI, um
# teardown de sessão, o OOM killer.
#
# O gate mede o TERMINAL, não o comando: um `check` por exit code ficaria verde
# sobre o bug, porque o processo morria na mesma e com o mesmo estado.
_ils="$("$BIN" image ls 2>&1)"; _ils_rc=$?
if ! grep -qE "^${IMG%%:*}[[:space:]:]" <<<"$_ils"; then
  skip "TTY reposto após um sinal" "sem a imagem $IMG no store — nada para exec (image ls rc=$_ils_rc: $(head -c 300 <<<"$_ils" | tr '\n' ' '))"
else
  TTYC="ttysig-$PFX"
  "$BIN" container run -d --net none --name "$TTYC" "$IMG" sleep 300 >/dev/null 2>&1
  check "um sinal repõe o terminal, e a morte continua a ser por sinal" ok \
    python3 - "$BIN" "$TTYC" <<'PYPROBE'
import os, pty, signal, subprocess, sys, termios, time

BIN, NAME = sys.argv[1], sys.argv[2]

def is_raw(a):
    return not (a[3] & termios.ECHO) and not (a[3] & termios.ICANON)

# Os quatro sinais que um operador ou um CI mandam. SIGKILL não entra: não é
# capturável, e prometer repor o terminal nesse caso seria mentira.
for sig in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP, signal.SIGQUIT):
    master, slave = pty.openpty()
    p = subprocess.Popen([BIN, "container", "exec", "-it", NAME, "sh"],
                         stdin=slave, stdout=slave, stderr=slave,
                         preexec_fn=os.setsid)
    time.sleep(2.5)
    if not is_raw(termios.tcgetattr(slave)):
        sys.exit(f"{sig}: a sessão nem chegou a pôr o terminal em raw")
    os.kill(p.pid, sig)
    try:
        p.wait(timeout=10)
    except subprocess.TimeoutExpired:
        p.kill(); p.wait(); sys.exit(f"{sig}: nao morreu")
    time.sleep(0.4)
    if is_raw(termios.tcgetattr(slave)):
        sys.exit(f"{sig}: o terminal FICOU em modo raw")
    # Re-raise com a disposição default: quem espera por este processo tem de
    # continuar a ver uma morte por sinal, não uma saída limpa.
    if p.returncode != -sig:
        sys.exit(f"{sig}: rc={p.returncode}, esperava {-sig}")
    os.close(master); os.close(slave)

# E o caminho normal não pode ter regredido — repõe por outro mecanismo (o
# `restore_mode` explícito) e propaga o código do workload.
master, slave = pty.openpty()
p = subprocess.Popen([BIN, "container", "exec", "-it", NAME, "sh", "-c", "exit 7"],
                     stdin=slave, stdout=slave, stderr=slave, preexec_fn=os.setsid)
p.wait(timeout=20)
if is_raw(termios.tcgetattr(slave)):
    sys.exit("saída normal: o terminal ficou em modo raw")
if p.returncode != 7:
    sys.exit(f"saída normal: rc={p.returncode}, esperava 7")
PYPROBE
  "$BIN" container rm -f "$TTYC" >/dev/null 2>&1
fi

section "contrato de output: o que a automação lê não pode mudar sozinho"
########################################
# Medido a 2026-08-26 antes de existir este bloco: as cinco propriedades abaixo
# JÁ se cumpriam. O que não existia era um gate — nada apanhava a regressão de
# nenhuma delas, e o que ninguém verifica é o que volta a partir-se.
#
# São propriedades do OUTPUT e não de um comando, por isso sobrevivem à
# reestruturação da CLI: quando `get` substituir estes `ls`, o bloco muda de
# alvo e não de sentido.

# As fixturas do bloco: um segredo com um valor reconhecível (para se poder
# provar que NÃO sai) e um manifesto que produz um plano com texto humano
# dentro — é aí que uma tradução escaparia para o JSON, não numa lista vazia.
E2E_SEC="outsec-$PFX"
"$BIN" secret create "$E2E_SEC" --from-literal senha=s3nha-do-gate >/dev/null 2>&1
E2E_OUTMF=$(mktemp "${TMPDIR:-/tmp}/e2e-out-XXXXXX.yaml")
cat > "$E2E_OUTMF" <<YAML
apiVersion: delonix.io/v1
kind: Volume
metadata: { name: outvol-$PFX }
spec: {}
YAML

# 1. Uma lista vazia continua a ser um ARRAY. Um `[]` que virasse `""` ou `null`
#    parte todo o `jq '.[]'` que exista lá fora, e parte-o em silêncio.
for g in "container ps" "image ls" "volume ls" "network ls" "vm ls" "secret ls"; do
  check "lista vazia de '$g' é um array JSON" ok bash -c \
    "'$BIN' $g -o json 2>/dev/null | python3 -c 'import json,sys; v=json.load(sys.stdin); assert isinstance(v, list)'"
done

# 2. O JSON não muda com a LÍNGUA. É a razão de ser do `-o json`: uma automação
#    que classifique por texto traduzido funciona na máquina onde foi escrita e
#    deixa de classificar num nó com outra locale — o mesmo defeito que os
#    códigos de saída existem para fechar, na outra ponta.
check "o -o json de uma listagem é idêntico em EN e PT" ok bash -c \
  "diff <('$BIN' volume ls -o json 2>/dev/null) <('$BIN' --l18n=pt volume ls -o json 2>/dev/null)"
check "o -o json de um plano é idêntico em EN e PT" ok bash -c \
  "diff <('$BIN' stack plan -f '$E2E_OUTMF' -o json 2>/dev/null) <('$BIN' --l18n=pt stack plan -f '$E2E_OUTMF' -o json 2>/dev/null)"

# 3. Sem ANSI quando o stdout não é um terminal. Um `| grep` que passe a apanhar
#    escapes deixa de casar, e a causa é invisível a olho nu.
check "num pipe não saem sequências ANSI" ok bash -c \
  "! '$BIN' container ps 2>/dev/null | grep -q \$'\033'"

# 4. Dados no stdout, tudo o resto no stderr. Um aviso que caia no stdout entra
#    no meio do JSON e o parser do outro lado rebenta.
check "o -o json não leva nada no stderr" ok bash -c \
  "[ -z \"\$('$BIN' image ls -o json 2>&1 >/dev/null)\" ]"

# 5. Um segredo nunca sai em claro sem se pedir. `secret ls` mostra os NOMES das
#    chaves e nunca os valores; o `inspect` redige e diz como revelar.
check "secret ls não traz valores" ok bash -c \
  "! '$BIN' secret ls -o json 2>/dev/null | grep -q 's3nha-do-gate'"
check "secret inspect redige por omissão" ok bash -c \
  "! '$BIN' secret inspect '$E2E_SEC' 2>/dev/null | grep -q 's3nha-do-gate'"
check "secret inspect --reveal mostra, e só então" ok bash -c \
  "'$BIN' secret inspect '$E2E_SEC' --reveal 2>/dev/null | grep -q 's3nha-do-gate'"

"$BIN" secret rm "$E2E_SEC" >/dev/null 2>&1
rm -f "$E2E_OUTMF"

########################################
section "verbos genéricos — encaminham, não reimplementam"
########################################
# A promessa do `cmd::verbs` é que `get networks` e `network ls` são a MESMA
# execução. Um teste unitário não o pode mostrar: prova-se comparando as duas
# saídas, e é a igualdade BYTE A BYTE que distingue encaminhar de reescrever
# parecido.
#
# O par `get pods | pod ls` saiu daqui porque `pod ls` já não existe (colapsado
# no B7). Deixá-lo virou `get pods` contra `get pods` — uma tautologia sempre
# verde, que é pior do que não ter o check: ocupa a linha do relatório e não
# compara nada. Entrou `image ls`, que existe e é um par a sério.
for par in "get:images|image:ls" "get:networks|network:ls" "get:volumes|volume:ls" \
           "get:secrets|secret:ls" "get:virtualmachines|vm:ls"; do
  novo_v="${par%%|*}"; velho_v="${par##*|}"
  # shellcheck disable=SC2086
  if [[ "$("$BIN" ${novo_v/:/ } -o json 2>&1)" == "$("$BIN" ${velho_v/:/ } -o json 2>&1)" ]]; then
    check "get ${novo_v#*:} == ${velho_v/:/ }" ok true
  else
    check "get ${novo_v#*:} == ${velho_v/:/ }" ok false
  fi
done
# Os Kinds que o `get` cobre, um a um — uma lista que encolhe em silêncio é
# indistinguível de uma que nunca cresceu.
for k in pods virtualmachines networks volumes secrets images \
         kubernetesclusters gateways httproutes; do
  check "get $k" ok "$BIN" get "$k"
done
# O container é a superfície IMPERATIVA (§3.3 da especificação), não declarativa.
check "get containers explica-se"  1 "$BIN" get containers
# Um formato que o grupo não sabe produzir é RECUSADO, não ignorado.
check "get -o json onde não há"    1 "$BIN" get kubernetesclusters -o json
# As três grafias de um Kind são a mesma pergunta.
check "get aceita o plural"       ok "$BIN" get pods
check "get aceita o singular"     ok "$BIN" get pod
check "get aceita a abreviatura"  ok "$BIN" get po
# Um Kind que não se pergunta assim DIZ porquê, e não responde vazio.
check "get stacks explica-se"     1 "$BIN" get stacks
check "get workloads explica-se"  1 "$BIN" get workloads
# Kind inexistente é 4 (não existe), não 1 genérico.
check "get de Kind inexistente"   4 "$BIN" get bananas
# E um delete sem nome nunca pode ser lido como «todos».
check "delete sem nome recusa"    1 "$BIN" delete pods

########################################
section "stack init --template --up (Sprint 7: --up passa a honrar o manifesto)"
########################################
# `--up` corria um `container run` à parte do manifesto gerado — ignorava
# rede/volumes/containers extra e até campos do PRÓPRIO container (memory/
# cpus/restart/readOnly/tmpfs). O template `odoo` já avisava no seu próprio
# comentário: "Odoo will not boot without the database, so `stack apply`
# (not a lone `container run`) is the way in" — `--up` violava isso à
# primeira. Corrigido para chamar `stack apply`, que também aplica o que o
# `container run` cru nunca aplicava a um único container.
SCAFN="scaf-$PFX"
SCAFDIR=$(mktemp -d "${TMPDIR:-/tmp}/e2e-scaffold-XXXXXX")
check "stack init --template httpd" ok "$BIN" stack init --template httpd "$SCAFDIR/$SCAFN"
check "o manifesto gerado declara memory/cpus" ok bash -c \
  "grep -q 'memory: 128M' '$SCAFDIR/$SCAFN/delonix-manifest.yaml'"
if timeout 180 "$BIN" stack init --template httpd "$SCAFDIR/$SCAFN" --up --force \
    >"${TMPDIR:-/tmp}/e2e-scaffold-up.log" 2>&1; then
  check "--up: o container tem memory_max do manifesto (não só o run cru)" ok bash -c \
    "$BIN container inspect '$SCAFN' | grep -q '\"memory_max\": \"128M\"'"
  check "--up: o container está mesmo a correr" ok bash -c \
    "$BIN container ls | grep -q '$SCAFN'"
  "$BIN" stack destroy -f "$SCAFDIR/$SCAFN/delonix-manifest.yaml" >/dev/null 2>&1
else
  skip "stack init --template httpd --up" \
    "build/apply não completou em 180s neste ambiente (rede lenta, ou porta 8080 já ocupada por outro processo do host — ver ${TMPDIR:-/tmp}/e2e-scaffold-up.log)"
  "$BIN" container rm -f "$SCAFN" >/dev/null 2>&1
fi
rm -rf "$SCAFDIR"

section "limpeza"
########################################
"$BIN" container rm -f "$C" >/dev/null 2>&1
check "volume rm" ok "$BIN" volume rm "$VOL"
check "network rm" ok "$BIN" network rm "$NET"

########################################
log ""
log "======================================"
log " PASS=$PASS  FAIL=$FAIL  SKIP=$SKIP  XFAIL=$XFAIL  XPASS=$XPASS"
log " detalhe: $OUT/results.jsonl"

# Os SKIP em BLOCO PRÓPRIO, e não diluídos nas seiscentas linhas acima. Um SKIP
# não chumba — a pré-condição faltou, e isso não diz nada sobre o motor — mas
# lido de passagem é indistinguível de um verde, e foi assim que cenários
# inteiros passaram despercebidos. Quem lê o resumo tem de ver o que NÃO foi
# exercitado sem ir procurar.
if (( SKIP > 0 )); then
  log ""
  log " $SKIP SKIP — o que NÃO foi exercitado (não chumba, mas também não prova nada):"
  for f in "${SKIPPED_NAMES[@]}"; do log "   ~ $f"; done
fi
if (( XFAIL > 0 )); then
  log ""
  log " $XFAIL XFAIL — chumbam por defeito CONHECIDO, com achado escrito:"
  for f in "${XFAIL_NAMES[@]}"; do log "   x $f"; done
fi
if (( XPASS > 0 )); then
  log ""
  log " $XPASS XPASS — marcados como defeito conhecido e PASSARAM. Tira a marca:"
  for f in "${XPASS_NAMES[@]}"; do log "   ! $f"; done
fi
if (( FAIL > 0 )); then
  log ""
  log " $FAIL falhas:"
  for f in "${FAILED_NAMES[@]}"; do log "   - $f"; done
fi
log "======================================"

# O código de saída.
#
# Até 2026-09-09 esta linha era `exit 0`, justificada com «o relatório é o
# produto; um FAIL não deve abortar a recolha». A primeira metade é verdade e
# não mudou — nada aqui aborta a meio, o `check` regista e segue. A segunda era
# um non-sequitur: o código de saída é a ÚLTIMA coisa que este script faz,
# depois de a recolha estar completa, e sair 0 com 36 FAIL tornava decorativo
# qualquer passo de CI construído por cima (medido: PASS=529 FAIL=36 SKIP=5, e
# `echo $?` a dizer 0).
#
# SKIP não chumba, de propósito: uma medição que não se pôde fazer não é um
# resultado negativo — a mesma regra que já governa o `schema publicado ==
# gerado` fora do checkout. Sai em destaque no bloco acima, que é onde tem de
# custar a ignorar.
if (( FAIL > 0 || XPASS > 0 )); then
  exit 1
fi
exit 0
