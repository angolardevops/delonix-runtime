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
# ## O que este número quer dizer, e o que NÃO quer (medido 2026-08-12)
#
# A CLI tem 245 comandos, 218 folhas invocáveis. Esta bateria verifica o `--help`
# de 100% delas (o ciclo dinâmico abaixo percorre a árvore) e EXECUTA 55 — 25%.
#
# **Actualizado 2026-08-15**: `net` (43 folhas em 6 subgrupos) tinha ZERO
# execuções e passou a ter 19 checks; `compose` tinha ZERO e passou a ter 20.
# `serve` tinha ZERO e passou a ter 17, `storage` ZERO e passou a ter 8 (dos
# quais 3 só num host com privilégio de montagem). Continuam sem nenhuma os
# comandos-folha
# `dash`/`man`/`version`.
# Cita-se a FRACÇÃO medida e a data, nunca o total de checks: um total que sobe
# faz a cobertura parecer melhor sem uma única folha nova exercitada.
# Os outros 163 têm o contrato verificado e nunca são corridos, concentrados em
# `net` (45), `image` (31) e `vm` (24). Um verde aqui lê-se com facilidade como
# «a CLI foi testada», e o que foi testado é sobretudo o texto de ajuda: foi em
# comandos nunca executados que a auditoria encontrou um errno cru (`node init`)
# e um `create` de overlay a sair 0 sobre uma rede por realizar.
#
# ## Isolamento: NÃO o faz por si, ao contrário do `chaos.sh`
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

PASS=0; FAIL=0; SKIP=0
declare -a FAILED_NAMES=()

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

skip() {
  SKIP=$((SKIP+1)); log "  SKIP  $1  — $2"
  python3 -c 'import json,sys; print(json.dumps({"name":sys.argv[1],"verdict":"SKIP","reason":sys.argv[2]}))' "$1" "$2" >>"$OUT/results.jsonl"
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
for g in container image build vm volumes network stack system cluster completion; do
  check "help de '$g'" ok "$BIN" "$g" --help
done
# Todos os subcomandos de cada grupo têm de ter --help funcional.
for g in container image vm volumes network stack system cluster; do
  subs=$("$BIN" "$g" --help 2>/dev/null | awk '/^(Commands|Subcommands):/{f=1;next} /^$/{f=0} f && $1 !~ /^-/ {print $1}')
  for s in $subs; do
    [[ "$s" == "help" ]] && continue
    check "help de '$g $s'" ok "$BIN" "$g" "$s" --help
  done
done

########################################
section "comandos de leitura (não destrutivos)"
########################################
check "container ls" ok "$BIN" container ls
check "container ls -a" ok "$BIN" container ls -a
check "container ls -q" ok "$BIN" container ls -q
check "image ls" ok "$BIN" image ls
check "image --vm ls" ok "$BIN" image --vm ls
check "volumes ls" ok "$BIN" volumes ls
check "network ls" ok "$BIN" network ls

# --- `-n/--namespace` FILTRA mesmo, e a coluna esconde-se (A-2/A-3) --------
# A armadilha que isto existe para apanhar é a que este repo já corrigiu três
# vezes (`--security-opt seccomp=`, `-v …:z`, `--network-alias`): uma flag
# ACEITE e depois IGNORADA. Um check do `--help` passaria com a filtragem por
# ligar — por isso estes EXECUTAM e comparam contagens.
ns_rows() { "$BIN" $1 2>/dev/null | tail -n +2 | grep -c . || true; }
for grupo in "container ps -a" "workload ls" "pod ls"; do
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
check "sem filtro a coluna NAMESPACE esconde-se" ok bash -c \
  "! '$BIN' container ps -a | head -1 | grep -q NAMESPACE"
check "com -n default a coluna aparece" ok bash -c \
  "'$BIN' container ps -a -n default | head -1 | grep -q NAMESPACE"
check "vm ls" ok "$BIN" vm ls
check "cluster ls" ok "$BIN" cluster ls
check "system info" ok "$BIN" system info
check "system df" ok "$BIN" system df
check "system events" ok "$BIN" system events
check "completion bash" ok "$BIN" completion bash

# --- os NOMES completam-se, e não só o script de registo (C-2) ------------
# O `completion bash` acima prova que o script de registo SAI; não prova que um
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
check "system restore completa caminhos" ok completa delonix system restore ""
# Este só vale onde o recurso existe — zero num host sem imagens VM é a resposta
# honesta, não uma falha, e um SKIP declarado conta como NÃO COBERTO.
if [ "$("$BIN" image vm ls 2>/dev/null | tail -n +2 | wc -l)" -gt 0 ]; then
  check "image vm rm completa (o destrutivo)" ok completa delonix image vm rm ""
else
  skip "image vm rm completa" "não há imagens VM neste host"
fi

########################################
section "erros: a CLI tem de RECUSAR o que é inválido"
########################################
check "container describe de inexistente recusa" fail "$BIN" container describe naoexiste-$PFX
check "container inspect de inexistente recusa" fail "$BIN" container inspect naoexiste-$PFX
check "volumes inspect de inexistente recusa" fail "$BIN" volumes inspect naoexiste-$PFX
check "network inspect de inexistente recusa" fail "$BIN" network inspect naoexiste-$PFX
check "container update sem mudanças recusa" fail "$BIN" container update naoexiste-$PFX
check "container stop de inexistente recusa" fail "$BIN" container stop naoexiste-$PFX
check "container rm de inexistente recusa" fail "$BIN" container rm naoexiste-$PFX
check "vm rm de inexistente recusa" fail "$BIN" vm rm naoexiste-$PFX
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
check "inexistente: volumes inspect diz 4" 4 "$BIN" volumes inspect naoexiste-$PFX
check "inexistente: network inspect diz 4" 4 "$BIN" network inspect naoexiste-$PFX
check "inexistente: secret rm diz 4" 4 "$BIN" secret rm naoexiste-$PFX
check "inexistente: vm rm diz 4" 4 "$BIN" vm rm naoexiste-$PFX
# O lote tem caminho de saída PRÓPRIO (`for_each_id` sai antes de o `main` ver o
# erro): sem a mesma classificação lá, `rm a b` respondia 1 onde `rm a` diz 4.
check "inexistente: lote de ids mantém a classe" 4 \
  "$BIN" container rm naoexiste1-$PFX naoexiste2-$PFX
# A classe não pode depender da língua — é essa a razão de existir do número.
check "inexistente em PT continua a dizer 4" 4 \
  "$BIN" --l18n=pt container inspect naoexiste-$PFX
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
VOL="vol-$PFX"
check "volumes create" ok "$BIN" volumes create "$VOL"
check "volumes create idempotente" ok "$BIN" volumes create "$VOL"
check "volumes ls mostra-o" ok bash -c "'$BIN' volumes ls | grep -q '$VOL'"
check "volumes inspect" ok "$BIN" volumes inspect "$VOL"
check "volumes describe" ok "$BIN" volumes describe "$VOL"
# Conflito (5): «o nome já está tomado» é uma resposta diferente de «o argumento
# está errado» — quem reconcilia adopta/salta no primeiro caso e pára no segundo.
check "snapshot create" ok "$BIN" volumes snapshot create "$VOL" --name s1
check "snapshot já existente diz 5 (conflito)" 5 "$BIN" volumes snapshot create "$VOL" --name s1
check "snapshot rm" ok "$BIN" volumes snapshot rm "$VOL" s1

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
check "volume pai para os shares" ok "$BIN" volumes create "$SHPAI"
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
  "$BIN" volumes apply -f "$SHWORK/shares.yaml"
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
  "$BIN" volumes apply -f "$SHWORK/velho.yaml"
# Sem backticks no padrão de propósito: o texto do erro tem-nos, e dentro das
# aspas duplas deste `bash -c` seriam substituição de comandos. Os `.` cobrem-nos.
check "a recusa nomeia a forma nova" ok \
  bash -c "'$BIN' volumes apply -f '$SHWORK/velho.yaml' 2>&1 | grep -q 'kind: Volume. with a .share:. block'"
check "sharevolume ls mostra os dois" ok \
  bash -c "test \$('$BIN' sharevolume ls | grep -c 'sh-$PFX') -eq 2"
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
# por engano.
SHDATA="$("$BIN" sharevolume describe "sh-$PFX" -n shteam-a | awk '/Mountpoint/{print $2}')"
[[ -n "$SHDATA" ]] && echo "dados-do-inquilino" >"$SHDATA/ficheiro.txt"
check "o describe do share resolveu um mountpoint" ok test -n "$SHDATA"
check "stack destroy alcança os dois shares" ok \
  "$BIN" stack destroy -f "$SHWORK/shares.yaml"
check "o destroy tirou-os do registo" ok \
  bash -c "test \$('$BIN' sharevolume ls | grep -c 'sh-$PFX') -eq 0"
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
  "$BIN" volumes apply -f "$SHWORK/mau.yaml"
"$BIN" volumes rm "$SHPAI" --force >/dev/null 2>&1

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
if "$BIN" image ls 2>/dev/null | grep -qF "$IMG "; then
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
  # em `crates/delonix-runtime/src/lib.rs`. Nenhum dos dois substitui o outro.
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
section "container em rede custom: hot reconfig pelo ingress"
########################################
CN="cn-$PFX"
NET2="net2-$PFX"
if "$BIN" network create "$NET2" --subnet 10.252.0.0/16 >/dev/null 2>&1 && \
   "$BIN" container run -d --name "$CN" --net "$NET" "$IMG" sleep 600 >/dev/null 2>&1; then
  check "update: net-connect a quente" ok "$BIN" container update "$CN" --net-connect "$NET2"
  check "update: rede extra no describe" ok bash -c "'$BIN' container describe '$CN' | grep -q '$NET2'"
  check "update: net-connect repetido recusa" fail "$BIN" container update "$CN" --net-connect "$NET2"
  check "update: net-rate a quente" ok "$BIN" container update "$CN" --net-rate 10mbit
  check "update: taxa inválida recusa" fail "$BIN" container update "$CN" --net-rate depressa
  check "update: net-rate-clear" ok "$BIN" container update "$CN" --net-rate-clear
  check "update: net-disconnect a quente" ok "$BIN" container update "$CN" --net-disconnect "$NET2"
  check "update: net-disconnect de rede não ligada recusa" fail "$BIN" container update "$CN" --net-disconnect "$NET2"
  "$BIN" container rm -f "$CN" >/dev/null 2>&1
else
  skip "hot reconfig em rede custom" "não foi possível criar rede/container em rede custom"
fi
"$BIN" network rm "$NET2" >/dev/null 2>&1

########################################
section "stack / manifesto"
########################################
WORK="$OUT/stack-$PFX"; mkdir -p "$WORK"
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
check "volumes describe do manifesto" ok "$BIN" volumes describe "sv-$PFX"

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
check "stack destroy --dry-run" ok "$BIN" stack destroy -f "$WORK/delonix-manifest.yaml" --dry-run
check "stack destroy" ok "$BIN" stack destroy -f "$WORK/delonix-manifest.yaml"
# O destroy levou o que a stack possui — o `describe` a seguir tem de correr na
# mesma (parte do ficheiro, não de um registo), mas os recursos já não existem.
check "volumes describe depois do destroy recusa" fail "$BIN" volumes describe "sv-$PFX"
"$BIN" volumes rm "sv-$PFX" >/dev/null 2>&1
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
check "…e o pull aparece classificado, não em silêncio" ok \
  bash -c "'$BIN' serve docker-api --matrix | grep '/images/create' | grep -qE 'refused|not written'"

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
check "…e já não está" fail "$BIN" volumes inspect "ov-$PFX"

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
  "$BIN" volumes inspect "hv2-$PFX"
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
  bash -c "! '$BIN' volumes inspect 'hv-$PFX' | grep -qi '9663676416'"
# O que NÃO voltou, e é dito em vez de escondido: um rollback não apaga sozinho.
check "o recurso criado depois SOBREVIVE a um rollback sem --prune" ok \
  "$BIN" volumes inspect "hv2-$PFX"
check "…e o rollback avisa que é preciso --prune para o levar" ok \
  bash -c "'$BIN' stack rollback --to 1 -f '$HWORK/hist.yaml' --dry-run 2>&1 | grep -q -- '--prune'"
check "rollback --prune leva-o" ok \
  "$BIN" stack rollback --to 1 -f "$HWORK/hist.yaml" --prune
check "…e agora já não está" fail "$BIN" volumes inspect "hv2-$PFX"
# Um rollback É um apply: ganha revisão própria, e a história diz de qual veio.
check "a história marca o rollback e diz que revisão replicou" ok \
  bash -c "'$BIN' stack history -f '$HWORK/hist.yaml' | grep -q 'rollback of 1'"

# A propriedade central do ADR-0019: o registo não é fonte de verdade nenhuma.
rm -rf "$DELONIX_ROOT/stacks"
check "sem stacks/: o plan continua a funcionar" ok "$BIN" stack plan -f "$HWORK/hist.yaml"
check "sem stacks/: o destroy continua a funcionar" ok "$BIN" stack destroy -f "$HWORK/hist.yaml"
check "sem stacks/: o history diz que não há, sem falhar" ok \
  "$BIN" stack history -f "$HWORK/hist.yaml"
"$BIN" volumes rm "hv-$PFX" >/dev/null 2>&1

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
section "schema gerado + explain + init"
########################################
check "schema print" ok "$BIN" schema print
check "schema print --kind Container" ok "$BIN" schema print --kind Container
check "schema print --kind inexistente recusa" fail "$BIN" schema print --kind NaoExiste
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
    "'$BIN' schema print | diff -q - '$SCHEMA_PUB'"
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
check "pod ls" ok "$BIN" pod ls

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
    - name: a
      image: $IMG
      command: ["sleep", "120"]
    - name: b
      image: $IMG
      command: ["sleep", "120"]
YAML
if "$BIN" pod create -f "$PODY" >/dev/null 2>"$OUT/pod-$PFX.err"; then
  check "pod create: o aviso de cgroup não se repete por membro" ok bash -c "
    n=\$(grep -c 'cgroup delegation' '$OUT/pod-$PFX.err' || true)
    [ \"\$n\" -le 1 ] || { echo \"o aviso saiu \$n vezes (um por membro)\"; exit 1; }
  "
  check "pod ls mostra-o" ok bash -c "'$BIN' pod ls | grep -q 'p$PFX'"
  check "pod describe" ok "$BIN" pod describe "p$PFX"
  check "pod rm -f" ok "$BIN" pod rm -f "p$PFX"
else
  skip "pod create + aviso de cgroup" "o pod create falhou (holder/SDN indisponível)"
fi
check "secret ls" ok "$BIN" secret ls
check "secret inspect inexistente recusa" fail "$BIN" secret inspect "nao-existe-$PFX"

########################################
section "vm (só o que não precisa de hipervisor)"
########################################
check "vm ls" ok "$BIN" vm ls
# `--disk`, e não `--image`: a flag `--image` NÃO EXISTE no `vm create`, por isso
# este check passava — esperava falha e obtinha falha — mas por «unexpected
# argument», nunca por a imagem não existir. Um teste que passa pela razão errada
# é pior que um teste em falta: dá cobertura por adquirida.
check "vm create com disco inexistente recusa" fail "$BIN" vm create "vm-$PFX" --disk /nao/existe.qcow2

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
  if "$BIN" vm create "$SVM" --disk "$SDISK" --backend libvirt --memory 256M >/dev/null 2>&1; then
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
      "'$BIN' vm snapshot restore '$SVM' s1 && '$BIN' vm status '$SVM' | grep -q Running"
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
    "$BIN" vm rm -f "$SVM" >/dev/null 2>&1
  else
    skip "vm: snapshot sobrevive a stop/start" "o vm create falhou neste host"
    "$BIN" vm rm -f "$SVM" >/dev/null 2>&1
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
    check "CH: create com a VM a correr RECUSA" fail "$BIN" vm snapshot create "$CVM" s1
    check "CH: e a recusa diz o que fazer" ok bash -c \
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
    check "CH: o ls responde com a VM a correr" ok bash -c \
      "'$BIN' vm snapshot ls '$CVM' | grep -qx s1"
    check "CH: rm com a VM a correr RECUSA" fail "$BIN" vm snapshot rm "$CVM" s1
    "$BIN" vm stop "$CVM" >/dev/null 2>&1
    check "CH: rm com a VM parada" ok "$BIN" vm snapshot rm "$CVM" s1
    check "CH: e saiu do disco" ok bash -c \
      "! qemu-img snapshot -l '$SROOT/vms/$CVM.qcow2' 2>/dev/null | grep -qw s1"
    "$BIN" vm rm -f "$CVM" >/dev/null 2>&1
  else
    skip "vm: snapshots no cloud-hypervisor" "o vm create CH falhou neste host (infra de rede?)"
    "$BIN" vm rm -f "$CVM" >/dev/null 2>&1
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
"$BIN" volumes create "$BKV" >/dev/null 2>&1
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
for _ in $(seq 50); do
  "$BIN" container exec "$BKC" sh -c 'echo prova > /data/f.txt' >/dev/null 2>&1
  "$BIN" container exec "$BKC" cat /data/f.txt 2>/dev/null | grep -q prova && break
  sleep 0.2
done

check "backup create --dry-run não escreve nada" ok "$BIN" backup create container "$BKC" --to "$BKDIR" --dry-run
check "backup create --dry-run mesmo não escreveu" ok bash -c "[[ -z \"\$(ls -A '$BKDIR')\" ]]"
check "backup create container" ok "$BIN" backup create container "$BKC" --to "$BKDIR"
check "o arquivo existe" ok bash -c "ls '$BKDIR'/container-$BKC-*.tar.gz >/dev/null"
check "o arquivo leva os dados do volume" ok bash -c \
  "tar tzf '$BKDIR'/container-$BKC-*.tar.gz | grep -q '^volumes/$BKV.tar.gz$'"
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
check "os dados voltaram" ok bash -c \
  "for _ in \$(seq 30); do '$BIN' container exec '$BKC' cat /data/f.txt 2>/dev/null | grep -q prova && exit 0; sleep 0.2; done; exit 1"

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
check "backup list mostra o arquivo" ok bash -c \
  "'$BIN' backup list --from '$BKDIR' | grep -q '$BKC'"
check "backup list --kind filtra" ok bash -c \
  "'$BIN' backup list --from '$BKDIR' --kind container | grep -q '$BKC'"
check "backup list --kind vm não traz um container" ok bash -c \
  "! '$BIN' backup list --from '$BKDIR' --kind vm | grep -q '$BKC'"
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
check "backup list conta o alheio como saltado" ok bash -c \
  "'$BIN' backup list --from '$BKDIR' | grep -q 'skipped\|saltado'"
rm -f "$BKDIR/alheio.tar.gz"

check "backup remove apaga o nosso" ok bash -c \
  "'$BIN' backup remove \$(basename \$(ls '$BKDIR'/container-$BKC-*.tar.gz | head -1)) --from '$BKDIR'"
check "e o arquivo desapareceu" ok bash -c \
  "! ls '$BKDIR'/container-$BKC-*.tar.gz >/dev/null 2>&1"

# O corte limpo: a forma antiga falha ALTO, nunca em silêncio (precedente v0.30.0).
check "o \`restore\` de raiz deixou de existir" 2 "$BIN" restore container x
check "o \`backup <kind>\` sem verbo deixou de existir" 2 "$BIN" backup container "$BKC"

# E o `system backup` NÃO foi dobrado aqui: é outro âmbito (o state root do nó),
# e o ADR-0020 chegou a classificá-lo como uma segunda porta para este grupo.
check "system backup continua a existir, separado" ok "$BIN" system backup --help
check "system restore continua a existir, separado" ok "$BIN" system restore --help

"$BIN" container rm -f "$BKC" >/dev/null 2>&1
"$BIN" volumes rm "$BKV" >/dev/null 2>&1

########################################
section "storage — o que se consegue provar sem uma NAS"

# O `storage` tinha ZERO execuções. E é o grupo onde o SKIP honesto é a maior
# parte do valor: `storage create` MONTA de imediato (`mount -t nfs|cifs|davfs`),
# o que exige CAP_SYS_ADMIN — num host rootless não é exercitável, ponto.
# Fingir com um servidor NFS falso provaria o parser, não o caminho.
STGN="stg-$PFX"

check "storage ls responde" ok "$BIN" storage ls
check "storage inspect de inexistente diz 4" 4 "$BIN" storage inspect "naoexiste-$PFX"
check "storage rm de inexistente diz 4" 4 "$BIN" storage rm "naoexiste-$PFX"
check "storage create com --type desconhecido recusa" 2 \
  "$BIN" storage create "$STGN" --type naoexiste --server 10.99.99.99 --share /x
check "storage create sem --share recusa" 2 \
  "$BIN" storage create "$STGN" --type nfs --server 10.99.99.99

# O que É exercitável do `create`: que ele falha ALTO por falta de privilégio e
# NÃO deixa estado atrás. Um create meio-feito é a classe que este repo já pagou
# várias vezes (o `create_network` sem rollback, o `volumes rm` a apagar a
# contabilidade antes dos dados) — e aqui está medido, não assumido.
if "$BIN" storage create "$STGN" --type nfs --server 10.99.99.99 --share /exports/x >/dev/null 2>&1; then
  # Um host COM privilégio de montagem chega aqui; então exercita-se o resto.
  check "storage inspect do que foi criado" ok "$BIN" storage inspect "$STGN"
  check "storage ls mostra-o" ok bash -c "'$BIN' storage ls | grep -q '$STGN'"
  check "storage rm" ok "$BIN" storage rm "$STGN"
else
  skip "storage create/inspect/rm com NAS real" "montar NFS/CIFS exige CAP_SYS_ADMIN — não exercitável em rootless"
  # E ISTO é o que se prova sem privilégio nenhum:
  check "um create falhado não deixa o directório do volume" ok bash -c \
    "[ ! -d \"\${DELONIX_ROOT:-\$HOME/.local/share/delonix}/volumes/$STGN\" ]"
  check "um create falhado não deixa registo em volumes ls" ok bash -c \
    "! '$BIN' volumes ls 2>/dev/null | grep -q '$STGN'"
  check "um create falhado não deixa registo em storage ls" ok bash -c \
    "! '$BIN' storage ls 2>/dev/null | grep -q '$STGN'"
fi

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
  pgrep -f "serve $sub --addr unix://$sock" | head -1
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

check "nenhum servidor desta corrida ficou para trás" ok bash -c \
  "! pgrep -f 'serve (cri|api|docker-api) --addr unix:///tmp/dlx-srv-.*$PFX' >/dev/null"

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
check "compose recusa deploy.replicas != 1" fail \
  "$BIN" compose config -f "$CWORK/replicas.yml" -p "cp$PFX"
check "e a recusa NOMEIA o campo" ok bash -c \
  "'$BIN' compose config -f '$CWORK/replicas.yml' -p 'cp$PFX' 2>&1 | grep -qi 'replicas'"

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
check "compose recusa profiles: por-serviço" fail \
  "$BIN" compose config -f "$CWORK/profiles.yml" -p "cp$PFX"
check "e a recusa NOMEIA os profiles" ok bash -c \
  "'$BIN' compose config -f '$CWORK/profiles.yml' -p 'cp$PFX' 2>&1 | grep -qi 'profiles'"

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
for leitura in "net ingress ls" "net egress ls" "net tunnel ls" "net httproute ls"; do
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
  check "net httproute ls" ok "$BIN" net httproute ls
  check "net tunnel ls" ok "$BIN" net tunnel ls
  check "net flow --help" ok "$BIN" net flow --help
  check "net boot status" ok "$BIN" net boot status
  # O grupo tem `status`, não `ls`. Fica fixado: a primeira versão deste check
  # assumiu `ls` (o verbo do resto da CLI) e chumbou com rc=2 — se algum dia o
  # `ls` for acrescentado, é uma escolha e não um acidente.
  check "net boot ls NÃO existe (é status)" 2 "$BIN" net boot ls

  # `net boot enable/disable` escreve units systemd em ~/.config/systemd/user,
  # que o DELONIX_ROOT NÃO redirecciona — num host com produção isso mexe fora
  # do isolamento. Fica declarado por cobrir, nunca corrido às escondidas.
  skip "net boot enable/disable" "escreve units em ~/.config/systemd/user, fora do DELONIX_ROOT"

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
if ! "$BIN" image ls 2>/dev/null | grep -qE "^${IMG%%:*}[[:space:]:]"; then
  skip "TTY reposto após um sinal" "sem a imagem $IMG no store — nada para exec"
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
for g in "container ps" "image ls" "volumes ls" "network ls" "vm ls" "secret ls"; do
  check "lista vazia de '$g' é um array JSON" ok bash -c \
    "'$BIN' $g -o json 2>/dev/null | python3 -c 'import json,sys; v=json.load(sys.stdin); assert isinstance(v, list)'"
done

# 2. O JSON não muda com a LÍNGUA. É a razão de ser do `-o json`: uma automação
#    que classifique por texto traduzido funciona na máquina onde foi escrita e
#    deixa de classificar num nó com outra locale — o mesmo defeito que os
#    códigos de saída existem para fechar, na outra ponta.
check "o -o json de uma listagem é idêntico em EN e PT" ok bash -c \
  "diff <('$BIN' volumes ls -o json 2>/dev/null) <('$BIN' --l18n=pt volumes ls -o json 2>/dev/null)"
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
# A promessa do `cmd::verbs` é que `get pods` e `pod ls` são a MESMA execução.
# Um teste unitário não o pode mostrar: prova-se comparando as duas saídas, e é
# a igualdade BYTE A BYTE que distingue encaminhar de reescrever parecido.
for par in "get:pods|pod:ls" "get:networks|network:ls" "get:volumes|volumes:ls" \
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

section "limpeza"
########################################
"$BIN" container rm -f "$C" >/dev/null 2>&1
check "volumes rm" ok "$BIN" volumes rm "$VOL"
check "network rm" ok "$BIN" network rm "$NET"

########################################
log ""
log "======================================"
log " PASS=$PASS  FAIL=$FAIL  SKIP=$SKIP"
log " detalhe: $OUT/results.jsonl"
if (( FAIL > 0 )); then
  log ""
  log " falhas:"
  for f in "${FAILED_NAMES[@]}"; do log "   - $f"; done
fi
log "======================================"
exit 0   # o relatório é o produto; um FAIL não deve abortar a recolha
