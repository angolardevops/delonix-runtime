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
# ## E são CINCO binários, não um — constrói-os todos no MESMO target
#
#   cargo build --release -p delonix-runtime-bin -p delonix-cri \
#     -p delonix-mgmt-bin -p delonix-mcp-bin -p delonix-node-api-bin
#
# O `delonix` não serve o CRI, a API de nó, o MCP nem a API de gestão: faz
# `exec` do irmão (`delonix_node::dispatch`/`exec_server`), e o `exec_server`
# prefere o irmão AO LADO do executável e só depois cai para o `PATH`. Logo uma
# corrida com só o `delonix` construído serve os binários INSTALADOS, de outra
# versão — e isso não aparece como «falta uma dependência», aparece como FAIL
# em troços do motor. Medido duas vezes: 6 FAIL no #655 (sem `-mgmt`/`-mcp`/
# `-node-api`) e FAIL=3 no troço do ADR-0074 D1 a 2026-10-08, este com o
# `delonix-cri` de 2026-09-07 a ser servido a um `delonix` de hoje.
#
# E passa o binário pelo CAMINHO do target, onde os cinco estão juntos:
#
#   ./scripts/e2e.sh "$CARGO_TARGET_DIR/release/delonix"
#
# **Nunca por um symlink solto em `target/release/delonix`**: aí o irmão não
# está ao lado do executável e o `exec_server` cai no `PATH` outra vez — o
# atalho parece equivalente e desfaz exactamente a precaução acima.
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
# ## O que o número quer dizer, e o que NÃO quer
#
# A bateria verifica o `--help` de **todas** as folhas invocáveis, e por
# CONSTRUÇÃO: o ciclo lê o inventário do `cli-tree.sh`, em vez da lista de
# grupos escrita à mão que lá estava (que dizia 100% e media 167 de 244 — 68%).
#
# **Quantas EXECUTA é outro número, e desde 2026-10-07 é um portão, não um
# comentário.** `scripts/cli_exec_ratchet.py` deriva-o do `results.jsonl` de uma
# corrida real — uma folha conta quando um `check`/`xfail` a invocou e julgou o
# resultado — e chumba quando desce, quando sobe sem a linha de base subir no
# mesmo commit, e quando uma folha NOVA entra na CLI sem decisão. Para o ver:
#
#   scripts/cli_exec_ratchet.py --list          # por grupo
#   scripts/cli_exec_ratchet.py --list-missing  # as que nunca correm
#
# Esta linha dizia «EXECUTA 91 — 37%», medido à mão a 2026-09-09 sobre a v3.0.0
# (244 folhas). A medição de 2026-10-07 deu 125 de 272 — 46,0%, e a de
# 2026-10-08, com as secções de cobertura deste commit, dá **160 de 272 —
# 58,8%**. Nenhum destes é comparável com o «91 — 37%»: esse foi contado à mão
# com uma regra que nunca ficou escrita. É precisamente por isso que a regra
# passou a estar no script e o número passou a ser derivado — um número guardado
# num comentário envelhece sem ninguém dar por isso, e este envelheceu um mês.
#
# Do salto de 126 para 160: 33 folhas que ninguém exercitava, e 1 que a
# bateria JÁ exercitava e o parser do ratchet não via (o `build`, invocado por
# dentro de uma função de shell). A correcção do parser — um `--help` numa segunda
# invocação do mesmo corpo suprimia a primeira — recupera 0 retroactivamente,
# medido contra a corrida completa anterior; existe para o `version` contar.
#
# E numa corrida ROOTLESS há folhas que o host não pode medir: um SKIP por
# pré-condição não conta folha nenhuma (o troço do `hostPort` em modo root/CNI
# quer root). O que o host não mede não é o que a bateria não exercita, e a
# diferença lê-se no bloco de SKIP do resumo.
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
# A árvore DESTE script, para os checks que leem `examples/`. Não deriva do
# `$BIN`: uma corrida com um binário de outra árvore continua a validar os
# exemplos que acompanham a bateria.
REPO="$(cd "$(dirname "$0")/.." && pwd)"
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
  # The node's providers file is machine state too. `install.sh` writes
  # `/etc/delonix/providers.yaml` with a `defaultProvider`, and a standing
  # default acts like an explicit `--backend` in every `vm create` of this
  # battery. Measured 2026-10-03: the same commit went from 0 to 1 FAIL
  # («vm create --require sem --backend…») between two runs, because the
  # installer had run in between and named libvirt. An empty providers file
  # keeps the outcome the engine's; a check that needs a provider sets its own.
  #
  # A providers file with no `networkDefaults` for a role switches off the
  # «single registered provider» rule for that role (ADR-0059 D3), so a run
  # that DOES configure a remote network provider through the environment has
  # to name it here. Measured 2026-10-03 against the lab node: with the bare
  # file every NetworkZone check of the remote-providers section answered
  # DX-1380 and nothing reached the cluster.
  if [[ -z "${DELONIX_PROVIDERS_CONFIG:-}" ]]; then
    {
      printf 'apiVersion: config.delonix.io/v1\n'
      if [[ -n "${DELONIX_PROXMOX_URL:-}" || -n "${DELONIX_OPNSENSE_URL:-}" ]]; then
        printf 'networkDefaults:\n'
        [[ -n "${DELONIX_PROXMOX_URL:-}" ]] && printf '  segment: proxmox\n'
        [[ -n "${DELONIX_OPNSENSE_URL:-}" ]] && printf '  gateway: opnsense\n'
      fi
    } >"$OUT/providers-none.yaml"
    export DELONIX_PROVIDERS_CONFIG="$OUT/providers-none.yaml"
  fi
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
  # The output and the command go through files, never argv: one argv string
  # is capped at 128 KiB (MAX_ARG_STRLEN), and on 2026-10-21 `manifest schema`
  # printed more than that — python3 never started («Argument list too long»),
  # the check still counted as PASS, and its line was missing from the JSONL. A
  # FAIL with a long output would have vanished the same way. `printf` is a
  # builtin, so writing the files has no such limit.
  local tmp="$OUT/.check.$BASHPID"
  printf '%s' "$out" >"$tmp.out"; printf '%s' "$*" >"$tmp.cmd"
  python3 - "$name" "$verdict" "$rc" "$tmp.out" "$tmp.cmd" >>"$OUT/results.jsonl" <<'PY'
import json,sys
name,verdict,rc,outf,cmdf=sys.argv[1:6]
out=open(outf,encoding="utf-8",errors="replace").read()
cmd=open(cmdf,encoding="utf-8",errors="replace").read()
# The END of the output is where a failing step says why (the `virsh` error
# under a `vm create`, the last line of a build), so a long output keeps its
# head and its tail, never only its head.
if len(out) > 4000:
    out = out[:1000] + "\n[... %d chars cut ...]\n" % (len(out) - 4000) + out[-3000:]
print(json.dumps({"name":name,"verdict":verdict,"rc":int(rc),"cmd":cmd,"output":out}))
PY
  rm -f "$tmp.out" "$tmp.cmd"
  if [[ $verdict == PASS ]]; then
    PASS=$((PASS+1)); log "  PASS  $name"
  else
    FAIL=$((FAIL+1)); FAILED_NAMES+=("$name")
    log "  FAIL  $name  (rc=$rc, esperado=$expect)"
    log "        $ $*"
    # Head AND tail. `head -8` alone cut a failing `vm create` before the
    # hypervisor's error: the first lines are progress, the reason comes last.
    local n; n=$(wc -l <<<"$out")
    if (( n > 16 )); then
      sed 's/^/        | /' <<<"$out" | head -6
      log "        | [… $((n - 12)) linhas cortadas — a cabeça e a cauda ficam em results.jsonl …]"
      sed 's/^/        | /' <<<"$out" | tail -6
    else
      sed 's/^/        | /' <<<"$out"
    fi
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

# `check` whose status IS the verdict, for a step the rest of a section stands
# on (a `vm create` the snapshot checks need). `check` itself always returns 0,
# which is why these steps used to be `if "$BIN" … >/dev/null; then … else skip`
# — and that `else skip` hid a real regression: on 2026-10-21 every libvirt
# `vm create` on main failed with "no network with matching name 'ingress'"
# and the three libvirt sections reported it as SKIP «o vm create falhou neste
# host», which the trace then filed under host properties. A create that fails
# on a host that HAS the hypervisor is a FAIL, with the error in the report.
#
# The rule holds for every setup step in this battery, not only libvirt's: a
# `network create`, a `container run`, a `stack apply`, a `build`, a `pod
# create` or a `serve cri` the rest of a block stands on goes through here. A
# block SKIPs only when a guard written from the HOST's side says the capability
# is absent — `libvirt_system_usable`, `ch_usable`, `host_has_subuid`,
# `E2E_HAVE_IMAGE`, `command -v crictl` — never because the engine's own step
# failed.
check_then() {
  local before=$FAIL
  check "$@"
  (( FAIL == before ))
}

# The host can run a libvirt VM: the two binaries the provider's `available()`
# demands, `qemu-img` for the test disks, and a usable `qemu:///system` (the
# NAT connection `vm create` picks when it can). Only when this is false may a
# libvirt section SKIP.
libvirt_system_usable() {
  command -v virsh >/dev/null && command -v qemu-img >/dev/null \
    && command -v qemu-system-x86_64 >/dev/null \
    && virsh -c qemu:///system list --all >/dev/null 2>&1
}

# The host can boot a Cloud Hypervisor VM: what the provider's own probe
# (`CloudHypervisorHost::probe`) asks for — the binary, `/dev/kvm` and one of
# the firmwares in `DEFAULT_CH_FIRMWARES` — plus `qemu-img` for the test disks,
# and `/dev/kvm` openable by THIS user, not only present. The CH sections used
# to guard on the binary alone and SKIP «o vm create CH falhou neste host (infra
# de rede?)» on anything else, a guess written into the report as a host
# property. Only when this is false may a CH section SKIP.
ch_usable() {
  command -v cloud-hypervisor >/dev/null && command -v qemu-img >/dev/null \
    && [[ -r /dev/kvm && -w /dev/kvm ]] || return 1
  [[ -n "${DELONIX_HYPERVISOR_FW:-}" && -e "$DELONIX_HYPERVISOR_FW" ]] && return 0
  local f
  for f in /usr/local/share/delonix/CLOUDHV.fd /usr/share/delonix/CLOUDHV.fd \
           /usr/local/share/delonix/hypervisor-fw /usr/share/delonix/hypervisor-fw; do
    [[ -e "$f" ]] && return 0
  done
  return 1
}
ch_missing() {  # the SKIP reason when `ch_usable` is false: the absent piece
  local m=()
  command -v cloud-hypervisor >/dev/null || m+=("cloud-hypervisor")
  command -v qemu-img >/dev/null || m+=("qemu-img")
  [[ -r /dev/kvm && -w /dev/kvm ]] || m+=("/dev/kvm legível e gravável")
  (( ${#m[@]} )) || m+=("um firmware de DEFAULT_CH_FIRMWARES")
  local out="${m[0]}" i
  for i in "${m[@]:1}"; do out+=", $i"; done
  printf 'sem %s' "$out"
}

# The host hands this user a subordinate uid range, which is what lets a
# container run as a uid other than 0 (ADR-0062). Read from `/etc/subuid` and
# `newuidmap`, never from the engine's own answer: the USER checks used to
# branch on what `container run … id -u` printed, so a broken USER mapping on a
# host WITH subuid read as «este host não mapeia um segundo uid» and SKIPped.
host_has_subuid() {
  command -v newuidmap >/dev/null \
    && grep -qE "^($(id -un)|$(id -u)):[0-9]+:[0-9]+" /etc/subuid 2>/dev/null
}

# The `serve cri` that `e2e_serve_up` started is up: a pid and its socket. A
# `serve cri` that does not come up on this host is the engine failing, not a
# missing host piece (the `crictl` guards above are the capability), so this is
# a judged step, with the server's log in the report when it fails.
cri_serving() {  # $1=label $2=pid $3=socket
  check_then "$1: serve cri sobe (pid e socket)" ok bash -c \
    "[ -n '$2' ] && [ -S '$3' ] || { echo 'pid=$2 socket=$3'; tail -8 '$SRVLOG' 2>/dev/null; exit 1; }"
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

# The teardown helpers (`e2e_rm`, `e2e_net_rm`, `e2e_image_rm`) live in their own file so the
# scenario that proves them under a real DX-8101 sources the very same code.
source "$(dirname "$0")/e2e_teardown.sh"
# shellcheck source=scripts/e2e_ports.sh
source "$(dirname "$0")/e2e_ports.sh"

[[ -x "$BIN" ]] || { log "binário não executável: $BIN"; exit 1; }
log "binário: $BIN"
log "versão:  $("$BIN" --version 2>&1)"

########################################
section "guarda: as portas que a bateria publica"
########################################
# Cada porta que a bateria publica no host vem do `e2e_port` (scripts/
# e2e_ports.sh). Antes vinham de `BASE + RANDOM % N`, e a base 51800 caía dentro
# do intervalo efémero: a 2026-10-21 saiu 51814, que era a ponta local de uma
# ligação de saída do host (127.0.0.1:51814↔:5433); o motor recusou bem o publish
# (EADDRINUSE) e isso deu 7 FAIL em cascata que nada tinham a ver com o diff. Um
# picker que erra assim volta a dar FAIL alheios, por isso prova-se aqui, antes
# de qualquer secção depender dele — e o segundo check reconstrói o caso exacto:
# uma porta presa por um socket LIGADO, não por um que escuta.
EPORTS="$(cd "$(dirname "$0")" && pwd)/e2e_ports.sh"
cat >"$OUT/ports-window.sh" <<'EOF'
source "$1"; log() { printf '%s\n' "$*" >&2; }
read -r lo hi </proc/sys/net/ipv4/ip_local_port_range
e2e_port p
# A host whose ephemeral range leaves no window (1024–65535) gets a kernel port.
(( lo - 20000 < 1000 && 65535 - hi < 1000 )) || (( p < lo || p > hi )) || { echo "p=$p dentro de $lo–$hi"; exit 1; }
_e2e_port_free "$p" || { echo "p=$p não está livre"; exit 1; }
EOF
check "e2e_port: a porta fica fora do intervalo efémero do host e está livre" ok bash "$OUT/ports-window.sh" "$EPORTS"
cat >"$OUT/ports-held.sh" <<'EOF'
source "$1"; log() { printf '%s\n' "$*" >&2; }
e2e_port h
ready="$2/ports-held.ready"; rm -f "$ready"
# The client end of a connection bound to $h, with a listener on a kernel port.
python3 -I -c '
import socket, sys, time
srv = socket.socket(); srv.bind(("127.0.0.1", 0)); srv.listen(1)
cli = socket.socket(); cli.bind(("127.0.0.1", int(sys.argv[1]))); cli.connect(srv.getsockname())
open(sys.argv[2], "w").close(); time.sleep(60)' "$h" "$ready" & holder=$!
trap 'kill $holder 2>/dev/null; rm -f "$ready"' EXIT
for _ in $(seq 1 50); do [[ -e "$ready" ]] && break; sleep 0.1; done
[[ -e "$ready" ]] || { echo "o socket ligado a $h não chegou a existir"; exit 1; }
_e2e_port_free "$h" && { echo "a sonda deu $h por livre com uma ligação nele"; exit 1; }
E2E_PORT_MIN=$h E2E_PORT_MAX=$h e2e_port q
[[ "$q" != "$h" ]] || { echo "e2e_port devolveu $h, que está preso"; exit 1; }
EOF
check "e2e_port: uma porta presa por uma ligação de saída é saltada" ok bash "$OUT/ports-held.sh" "$EPORTS" "$OUT"
cat >"$OUT/ports-distinct.sh" <<'EOF'
source "$1"; log() { printf '%s\n' "$*" >&2; }
e2e_port a
E2E_PORT_MIN=$a E2E_PORT_MAX=$((a + 1))
E2E_PORTS_GIVEN=" "; e2e_port x; e2e_port y; e2e_port z
[[ "$x" != "$y" && "$z" != "$x" && "$z" != "$y" ]] || { echo "repetida: $x $y $z"; exit 1; }
EOF
check "e2e_port: a mesma corrida nunca recebe a mesma porta duas vezes" ok bash "$OUT/ports-distinct.sh" "$EPORTS"

########################################
section "guard: the image teardown judges its effect"
########################################
# `e2e_image_rm` is what turns a leftover test image into a FAIL. Until
# 2026-10-22 three of its call sites were `image rm … >/dev/null 2>&1`, a verb
# the CLI does not have, and no run noticed. A stub engine proves the helper
# itself: the stub's `image ls` lists whatever `$STUB_LS` says, and its
# `image remove` answers `$STUB_RC`.
ETEARDOWN="$(cd "$(dirname "$0")" && pwd)/e2e_teardown.sh"
cat >"$OUT/image-rm-stub" <<'EOF'
#!/usr/bin/env bash
case "$1 $2${3:+ $3}" in
  "image remove "*) exit "${STUB_RC:-0}" ;;
  "image ls -o")
    [[ -n "${STUB_ERR:-}" ]] && echo "$STUB_ERR" >&2
    [[ -n "${STUB_BAD:-}" ]] && { echo "not json"; exit 0; }
    printf '[{"id":"abc123","repo_tags":["%s"]}]\n' "${STUB_LS:-}" ;;
  *) echo "error: unrecognized subcommand '$2'" >&2; exit 2 ;;
esac
EOF
chmod +x "$OUT/image-rm-stub"
cat >"$OUT/image-rm.sh" <<'EOF'
BIN="$2"; source "$1"; e2e_image_rm "$3"
EOF
_irm=(bash "$OUT/image-rm.sh" "$ETEARDOWN" "$OUT/image-rm-stub" "t-$PFX:1")
check "e2e_image_rm: a removed name that image ls no longer lists passes" ok env STUB_LS=alpine:3.19 "${_irm[@]}"
check "e2e_image_rm: an image that is already gone (4) passes" ok env STUB_RC=4 "${_irm[@]}"
check "e2e_image_rm: a removal the CLI refuses (rc 2) fails" 1 env STUB_RC=2 "${_irm[@]}"
check "e2e_image_rm: a name image ls still lists, even behind a registry, fails" 1 env STUB_LS="localhost/t-$PFX:1" "${_irm[@]}"
# The table cuts a long REPOSITORY:TAG with «…»; the helper reads JSON so it
# still sees one.
_irml=(bash "$OUT/image-rm.sh" "$ETEARDOWN" "$OUT/image-rm-stub" "t-$PFX-$(printf 'y%.0s' {1..60}):1")
check "e2e_image_rm: a long name image ls still lists fails" 1 env STUB_LS="localhost/${_irml[4]}" "${_irml[@]}"
# A warning on stderr is not part of the listing, and a listing that does not
# parse is a failure, never «the name is gone».
check "e2e_image_rm: a warning on stderr does not hide a name still listed" 1 env STUB_ERR=warning STUB_LS="t-$PFX:1" "${_irm[@]}"
check "e2e_image_rm: a listing that does not parse fails" 1 env STUB_BAD=1 "${_irm[@]}"

########################################
section "guarda: o irmão SERVIDO é da release do \$BIN"
########################################
# O cabeçalho desta bateria já diz que são CINCO binários e porquê. Isto MEDE-O,
# porque o aviso existia e não bastou: a 2026-10-08 uma corrida com só o
# `delonix` construído deu FAIL=3 no troço do ADR-0074 D1, e a causa não era o
# motor — o `exec_server` caiu para o `PATH` e serviu um `delonix-cri` de
# 2026-09-07, um mês anterior à recusa que aqueles checks verificam. Um aviso
# protege quem o lê; um guarda protege quem não leu.
#
# **Porque não basta a verificação que o motor já tem.** O
# `delonix_node::dispatch::check_version` existe precisamente para isto e diz-se
# «a server left on the PATH from an older install». Mas é o SERVIDOR que a
# corre, logo um servidor obsoleto não a corre: medido no mesmo binário de
# 2026-09-07, com `DELONIX_DISPATCH_VERSION` deliberadamente errado ele arrancou
# de qualquer forma, e a string da variável não existe sequer no executável.
# Uma verificação do lado do CHAMADO falha no único caso para que existe —
# quanto mais velho o irmão, menos protege. Daí o guarda ser aqui, do lado de
# quem mede.
#
# Afirma a IDENTIDADE, não a presença: o irmão de Setembro EXISTIA, e é isso que
# o torna invisível a um `command -v`. E identidade é a RELEASE, não o caminho —
# um irmão instalado em `/usr/local/bin` da mesma versão é equivalente e passa
# (medido), porque o que estraga a medição é a versão divergente, não o sítio.
#
# E o `--version` tem de ser domado, porque num irmão obsoleto ele ARRANCA O
# SERVIDOR em vez de imprimir (medido: rc=124 ao fim de 8 s, log
# «delonix-cri starting»). Daí, em cada sonda: `timeout --kill-after` para o
# grupo não sobreviver, um socket descartável e CURTO (o `sun_path` são 108
# bytes, e o caminho de um scratchpad já passa dos 90) e um `rm -f` a seguir —
# sem ele ficava um socket por corrida em `/tmp`, medido. Um timeout NÃO é
# «sem veredicto»: os 4.x+ respondem (`tests/version_flag.rs`), logo um irmão
# que não responde é obsoleto POR ESSE FACTO, e isso chumba fechado.

# A versão que o irmão servido tem de ter, ou vazio se o próprio $BIN não a diz.
e2e_sibling_version() {  # $1=caminho do irmão → ecoa a versão, ou vazio
  local sock="/tmp/dlx-sg-$$.sock" v
  v="$(DELONIX_CRI_ADDR="unix://$sock" DELONIX_API_ADDR="unix://$sock" \
       DELONIX_NODE_API_ADDR="unix://$sock" \
       timeout --kill-after=2 6 "$1" --version 2>/dev/null | head -1 | awk '{print $2}')"
  rm -f "$sock"
  printf '%s' "$v"
}

# O que o `exec_server` escolheria: o irmão AO LADO do executável e só depois o
# `PATH` — a mesma ordem, senão o guarda mede um binário e a bateria serve outro.
e2e_sibling_path() {  # $1=nome → ecoa o caminho que seria servido, ou vazio
  local beside; beside="$(dirname "$BIN")/$1"
  if [ -f "$beside" ]; then printf '%s' "$beside"; else command -v "$1" 2>/dev/null || true; fi
}

E2E_BIN_VER="$("$BIN" --version 2>/dev/null | head -1 | awk '{print $2}')"
if [ -z "$E2E_BIN_VER" ]; then
  skip "o irmão servido é da release do \$BIN" "o \$BIN não diz a versão — nada com que comparar"
else
  log "  versão de referência: $E2E_BIN_VER"
  for _sib in delonix-cri delonix-mgmt delonix-mcp delonix-node-api; do
    _sp="$(e2e_sibling_path "$_sib")"
    if [ -z "$_sp" ]; then
      # As secções que o precisam já saltam com a sua própria razão; aqui não há
      # nada a medir, e um FAIL diria «está errado» onde não está.
      skip "o $_sib servido é da release do \$BIN" \
        "não está ao lado do \$BIN nem no PATH — nada será servido"
      continue
    fi
    _sv="$(e2e_sibling_version "$_sp")"
    # A razão vai no NOME porque é o que aparece no resumo: um FAIL aqui tem de
    # dizer «mediste o binário errado», não deixar N secções a chumbar por uma
    # causa que não é a delas.
    check "o $_sib servido é da release do \$BIN ($_sp diz ${_sv:-NADA})" \
      ok bash -c "[ '${_sv}' = '${E2E_BIN_VER}' ]"
  done
  unset _sib _sp _sv
fi

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
check "vm image ls" ok "$BIN" vm image ls
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
# A travessia paralela do disco TEM de dar o mesmo que a sequencial — é este
# número que impõe a quota rootless. Compara-se a SAÍDA inteira, não só o rc.
check "system df: paralelo e sequencial dão o mesmo" ok bash -c '
  a=$('"'$BIN'"' system df 2>/dev/null)
  b=$(DELONIX_WALK_THREADS=1 '"'$BIN'"' system df 2>/dev/null)
  [ -n "$a" ] && [ "$a" = "$b" ]'
# `net netns gc`: sem `--force` e sem terminal, RECUSA-SE — nunca termina nada
# por engano; e o relatório existe mesmo sem nada para reclamar.
check "net netns gc --help" ok "$BIN" net netns gc --help
check "net netns gc sem --force nunca termina nada" ok bash -c '
  out=$('"'$BIN'"' net netns gc </dev/null 2>&1); rc=$?
  [ $rc -eq 0 ] || echo "$out" | grep -q -- "--force"'
check "system events" ok "$BIN" system events
check "completion shell bash" ok "$BIN" completion shell bash

# --- os NOMES completam-se, e não só o script de registo (C-2) ------------
# O `completion shell bash` acima prova que o script de registo SAI; não prova que um
# TAB sobre um argumento sugere alguma coisa. A distinção não é teórica: o
# `vm image rm` — o comando DESTRUTIVO — não sugeria nada enquanto o `describe`
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
if [ "$("$BIN" vm image ls 2>/dev/null | tail -n +2 | wc -l)" -gt 0 ]; then
  check "vm image rm completa (o destrutivo)" ok completa delonix vm image rm ""
else
  skip "vm image rm completa" "não há imagens VM neste host"
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
# The repository's own sample stack (web + Postgres + Redis + worker, shared
# settings through `<<: *anchor`, two networks on one service, `name:`). Every
# key it uses is served, so the assessment is clean; and `compose config`
# names the project from `name:` and keeps the second network. Three defects
# this sample found (2026-10-11) are the three greps: `<<` refused,
# `name:` ignored, the second network dropped with a warning.
SAMPLE="$REPO/examples/compose-webapp/compose.yaml"
check "migrate assess: the compose-webapp sample is fully served" 0 \
  "$BIN" migrate assess -f "$SAMPLE" --detailed-exitcode
check "compose config: the sample's project is its name: and web keeps both networks" ok bash -c \
  "out=\$('$BIN' compose config -f '$SAMPLE' 2>&1) && grep -q '^# compose project: shop' <<<\"\$out\" \
   && grep -A4 '^service web:' <<<\"\$out\" | grep -q 'extra networks: \\[\"shop_backend\"\\]'"
rm -rf "$COMPW"

########################################
section "provider ls / describe / matrix (ADR-0050): a matriz medida, não afirmada"
# O que se prova aqui é a LIGAÇÃO: que o verbo lê as declarações reais, que o
# JSON leva os campos que o contrato (`ProviderInfo`) leva, que um `supported`
# nunca vem sem evidência, e que a matriz publicada é a gerada — o teste
# unitário do bin compara o ficheiro, este check confirma-o contra o binário.
check "provider ls" ok "$BIN" provider ls
# The count comes from `provider matrix`, not a literal: ADR-0052 added a seventh
# provider and a `==6` written here went red on every run after it.
check "provider ls -o json lista os mesmos providers que a matriz" ok bash -c \
  "n=\$('$BIN' provider matrix | grep -c '^- \*\*'); '$BIN' provider ls -o json | python3 -c 'import json,sys; v=json.load(sys.stdin); n=int(sys.argv[1]); assert n>0 and len(v)==n, (len(v), n)' \"\$n\""
check "provider ls -o json: cada capacidade leva name/supported/state/detail" ok bash -c \
  "'$BIN' provider ls -o json | python3 -c '
import json,sys
for p in json.load(sys.stdin):
    assert p[\"id\"] and p[\"kind\"] in (\"compute\",\"network\",\"storage\",\"gateway\"), p
    assert p[\"health\"][\"reason\"], p[\"id\"]
    for c in p[\"capabilities\"]:
        assert set(c) >= {\"name\",\"supported\",\"state\",\"detail\",\"domain\"}, c
        assert c[\"state\"] != \"supported\" or c[\"detail\"], (p[\"id\"], c[\"name\"])
'"
check "provider ls --kind network só traz a rede" ok bash -c \
  "'$BIN' provider ls --kind network -o json | python3 -c 'import json,sys; v=json.load(sys.stdin); assert v and all(p[\"kind\"]==\"network\" for p in v), [p[\"kind\"] for p in v]'"
# ADR-0059 F1: the perimeter appliance is a `gateway` provider, listed whether or
# not a target is registered, and it answers the network rows (catalog 1.1.0).
check "provider ls --kind gateway traz o opnsense e responde às linhas de rede" ok bash -c \
  "'$BIN' provider ls --kind gateway -o json | python3 -c 'import json,sys; v=json.load(sys.stdin); assert [p[\"id\"] for p in v]==[\"opnsense\"] and all(p[\"kind\"]==\"gateway\" for p in v), v; assert any(c[\"name\"]==\"net.gateway.filter\" for c in v[0][\"capabilities\"])'"
check "provider describe opnsense" ok "$BIN" provider describe opnsense
check "provider describe libvirt" ok "$BIN" provider describe libvirt
check "provider describe linux --kind storage" ok "$BIN" provider describe linux --kind storage
check "provider describe de um provider inexistente diz 4" 4 "$BIN" provider describe naoexiste
check "provider ls --kind inválido recusa" fail "$BIN" provider ls --kind ceph
check "provider matrix é a matriz publicada, byte a byte" ok bash -c \
  "diff <('$BIN' provider matrix) '$(dirname "$0")/../docs/providers/capability-matrix.md' >/dev/null"
check "provider ls --l18n=pt traduz o cabeçalho" ok bash -c \
  "'$BIN' --l18n=pt provider ls | grep -q 'medidos neste host'"
# `--probe` (ADR-0049 fatia 3) só existe para o provider remoto: num local
# seria uma flag que não faz nada, e sem alvo configurado a classe é a do
# «indisponível» (69) — nunca um 0 com a secção do cluster em falta.
check "provider describe libvirt --probe recusa (1)" 1 "$BIN" provider describe libvirt --probe
check "provider describe proxmox --probe sem alvo diz 69" 69 env -u DELONIX_PROXMOX_URL "$BIN" provider describe proxmox --probe

# ADR-0050 D5: the same providers through the node CONTRACT. `serve node-api`
# runs the sibling `delonix-node-api` (built beside the CLI in this checkout);
# what the check reads is the JSON on the socket, and it has to be the same set
# of providers `provider ls` lists — measured on this host by the same probes.
NODESOCK="/tmp/dlxe2e-node-$$.sock"
NODEBIN="$(dirname "$BIN")/delonix-node-api"
if [[ -x "$NODEBIN" ]]; then
  "$BIN" serve node-api --addr "unix://$NODESOCK" >"$OUT/node-api.log" 2>&1 &
  NODEPID=$!
  for _ in $(seq 100); do [[ -S "$NODESOCK" ]] && break; sleep 0.05; done
  check "serve node-api: o socket existe e é 0600" ok bash -c \
    "[[ -S '$NODESOCK' ]] && [[ \$(stat -c %a '$NODESOCK') == 600 ]]"
  check "GET /v1/providers responde com os mesmos providers que provider ls" ok bash -c \
    "diff <(curl -s --unix-socket '$NODESOCK' http://localhost/v1/providers | python3 -c 'import json,sys; print(sorted(p[\"kind\"]+\"/\"+p[\"id\"] for p in json.load(sys.stdin)[\"providers\"]))') \
          <('$BIN' provider ls -o json | python3 -c 'import json,sys; print(sorted(p[\"kind\"]+\"/\"+p[\"id\"] for p in json.load(sys.stdin)))')"
  check "GET /v1/providers: cada capacidade leva name/supported/state/detail e o catalog_version" ok bash -c \
    "curl -s --unix-socket '$NODESOCK' http://localhost/v1/providers | python3 -c 'import json,sys; d=json.load(sys.stdin); assert all(set(c)>={\"name\",\"supported\",\"state\",\"detail\"} for p in d[\"providers\"] for c in p[\"capabilities\"]); assert all(p[\"catalog_version\"] for p in d[\"providers\"])'"
  check "GET /v1/providers?kind=network só traz a rede" ok bash -c \
    "curl -s --unix-socket '$NODESOCK' 'http://localhost/v1/providers?kind=network' | python3 -c 'import json,sys; d=json.load(sys.stdin)[\"providers\"]; assert d and all(p[\"kind\"]==\"network\" for p in d)'"
  check "GET /v1/providers?kind=gateway traz o opnsense (ADR-0059 F1)" ok bash -c \
    "curl -s --unix-socket '$NODESOCK' 'http://localhost/v1/providers?kind=gateway' | python3 -c 'import json,sys; d=json.load(sys.stdin)[\"providers\"]; assert [p[\"id\"] for p in d]==[\"opnsense\"], d'"
  check "GET /v1/providers?kind=ceph é 400 com um problem+json DX-1000 (gRPC 3)" ok env \
    PROB_HDR="$(curl -s -D - -o /dev/null --unix-socket "$NODESOCK" 'http://localhost/v1/providers?kind=ceph')" \
    PROB_JSON="$(curl -s --unix-socket "$NODESOCK" 'http://localhost/v1/providers?kind=ceph')" \
    python3 -c 'import json,os; h=os.environ["PROB_HDR"].lower(); assert "http/1.1 400" in h and "content-type: application/problem+json" in h, h; d=json.loads(os.environ["PROB_JSON"]); assert (d["status"],d["dx"],d["code"],d["grpc_status"])==(400,"DX-1000","DX_INVALID_ARGUMENT",3), d'
  # BUG REAL, medido na 1.ª corrida deste check: o fallback do router do tonic
  # respondia a QUALQUER caminho desconhecido com 200 + `grpc-status: 12` e corpo
  # vazio — um cliente REST lia «servido, sem nada». Agora é 404 com um problem
  # document (DX-4001, ADR-0042 D2); só um chamador gRPC recebe o UNIMPLEMENTED de fio.
  check "um caminho sem handler é 404 com um problem+json DX-4001, nunca um 200 vazio" ok env \
    PROB_HDR="$(curl -s -D - -o /dev/null --unix-socket "$NODESOCK" http://localhost/v1/nada)" \
    PROB_JSON="$(curl -s --unix-socket "$NODESOCK" http://localhost/v1/nada)" \
    python3 -c 'import json,os; h=os.environ["PROB_HDR"].lower(); assert "http/1.1 404" in h and "content-type: application/problem+json" in h, h; d=json.loads(os.environ["PROB_JSON"]); assert (d["status"],d["dx"],d["instance"])==(404,"DX-4001","/v1/nada"), d'
  check "um método que o caminho não serve é 405 com Allow: GET e um problem+json" ok env \
    PROB_HDR="$(curl -s -X DELETE -D - -o /dev/null --unix-socket "$NODESOCK" http://localhost/v1/node)" \
    python3 -c 'import os; h=os.environ["PROB_HDR"].lower(); assert "http/1.1 405" in h and "allow: get" in h and "content-type: application/problem+json" in h, h'
  # ADR-0042 passo C: GetNodeInfo/GetHealth/GetCapacity pelas rotas JSON, e o
  # OpenAPI publicado. Cada resposta é comparada com o que a CLI diz no mesmo
  # host; os valores passam às asserções por variáveis de ambiente.
  check "GET /v1/node: a versão e o commit são os do delonix version" ok env \
    NODE_JSON="$(curl -s --unix-socket "$NODESOCK" http://localhost/v1/node)" \
    WANT_VERSION="$("$BIN" version | head -1 | awk '{print $2}')" \
    WANT_COMMIT="$("$BIN" version | grep -o 'commit: [0-9a-f]*' | awk '{print $2}')" \
    python3 -c 'import json,os; d=json.loads(os.environ["NODE_JSON"]); assert d["api_version"]=="delonix.node.v1", d; assert d["engine_version"]==os.environ["WANT_VERSION"], d; assert d["engine_commit"]==os.environ["WANT_COMMIT"], d'
  check "GET /v1/node/health: quatro condições, e a do cgroup concorda com o system info" ok env \
    HEALTH_JSON="$(curl -s --unix-socket "$NODESOCK" http://localhost/v1/node/health)" \
    SYSINFO="$("$BIN" --l18n=en system info 2>/dev/null | grep 'cgroup2 delegated')" \
    python3 -c 'import json,os; d=json.loads(os.environ["HEALTH_JSON"]); c={x["type"]:x["status"] for x in d["conditions"]}; assert list(c)==["NetworkReady","StoreWritable","CgroupDelegated","ProvidersAvailable"], d; want="CONDITION_STATUS_TRUE" if "yes" in os.environ["SYSINFO"] else "CONDITION_STATUS_FALSE"; assert c["CgroupDelegated"]==want, (c, os.environ["SYSINFO"])'
  check "GET /v1/node/capacity: cada número é medido ou está em unmeasured, nunca um zero calado" ok env \
    CAP_JSON="$(curl -s --unix-socket "$NODESOCK" http://localhost/v1/node/capacity)" \
    python3 -c 'import json,os; d=json.loads(os.environ["CAP_JSON"]); u=set(d["unmeasured"]); bad=[k for k in ("cpu_millis_total","cpu_millis_allocatable","memory_bytes_total","memory_bytes_allocatable","pids_allocatable") if int(d[k])<=0 and k not in u]; assert not bad, (bad, d)'
  check "GET /openapi.json serve o documento gerado, com as rotas do NodeService" ok env \
    DOC_JSON="$(curl -s --unix-socket "$NODESOCK" http://localhost/openapi.json)" \
    python3 -c 'import json,os; d=json.loads(os.environ["DOC_JSON"]); assert d["openapi"].startswith("3"); assert {"/v1/node","/v1/node/health","/v1/node/capacity","/v1/providers"} <= set(d["paths"])'
  NODE_ASSETS_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
  # ADR-0042 D2 (Richardson 3): a client navega a partir de GET /v1 pelos links,
  # sem construir URIs — cada link oferecido responde 200, e o cabeçalho Link
  # (RFC 8288) espelha os links do corpo.
  check "GET /v1: cada link oferecido responde 200, e o cabeçalho Link espelha o corpo" ok env \
    ROOT_HDR="$(curl -s -D - -o /dev/null --unix-socket "$NODESOCK" http://localhost/v1)" \
    ROOT_JSON="$(curl -s --unix-socket "$NODESOCK" http://localhost/v1)" \
    NODESOCK="$NODESOCK" \
    python3 -c 'import json,os,subprocess; d=json.loads(os.environ["ROOT_JSON"]); assert d["api_version"]=="delonix.node.v1", d; hdr=[l for l in os.environ["ROOT_HDR"].splitlines() if l.lower().startswith("link:")]; assert hdr and "rel=\"self\"" in hdr[0], hdr
for l in d["links"]:
    code=subprocess.run(["curl","-s","-o","/dev/null","-w","%{http_code}","--unix-socket",os.environ["NODESOCK"],"http://localhost"+l["href"]],capture_output=True,text=True).stdout
    assert code=="200", (l, code)'
  # ADR-0042 passo C: as rotas REST são GERADAS das anotações google.api.http.
  # Uma rota do contrato que o motor ainda não serve é 501 (DX-6001), nunca o
  # 404 de um caminho que o contrato não tem; um verbo (`{name}:start`) resolve.
  check "uma rota do contrato ainda não servida é 501 DX-6001, e um verbo noutro método é 405 com Allow: POST" ok env \
    NS_JSON="$(curl -s -X POST --unix-socket "$NODESOCK" http://localhost/v1/namespaces/default/containers/web:start)" \
    NS_HDR="$(curl -s -D - -o /dev/null --unix-socket "$NODESOCK" http://localhost/v1/namespaces/default/containers/web:start)" \
    python3 -c 'import json,os; d=json.loads(os.environ["NS_JSON"]); assert (d["status"],d["dx"],d["grpc_status"])==(501,"DX-6001",12), d; h=os.environ["NS_HDR"].lower(); assert "http/1.1 405" in h and "allow: post" in h, h'
  check "um parâmetro de query que o pedido não tem é recusado (400), não ignorado" ok bash -c \
    "[[ \$(curl -s -o /dev/null -w '%{http_code}' --unix-socket '$NODESOCK' 'http://localhost/v1/providers?kindd=network') == 400 ]]"
  # ADR-0042 passo E, 1.ª onda: as leituras de rede pelo contrato. Duas redes
  # neste root — uma aplicada por manifesto (leva o carimbo de posse como
  # label), outra criada pela CLI (sem labels) — e o socket tem de as mostrar
  # como o motor as tem.
  printf '%s\n' 'apiVersion: networking.delonix.io/v1alpha1' 'kind: Network' \
    'metadata: { name: e1net-a }' 'spec: {}' > "$OUT/e1-nets.yaml"
  "$BIN" stack apply -f "$OUT/e1-nets.yaml" >/dev/null 2>&1
  "$BIN" network create e1net-b >/dev/null 2>&1
  NETS="http://localhost/v1/namespaces/default/networks"
  check "GET …/networks lista as redes do nó, com a condição Realized lida do registo do dataplane" ok env \
    NET_JSON="$(curl -s --unix-socket "$NODESOCK" "$NETS")" \
    python3 -c 'import json,os; d=json.loads(os.environ["NET_JSON"]); n={x["meta"]["name"]:x for x in d["networks"]}; assert {"e1net-a","e1net-b"}<=set(n), list(n); a=n["e1net-a"]; assert a["meta"]["namespace"]=="default" and a["meta"]["labels"].get("delonix.io/managed-by")=="delonix" and not n["e1net-b"]["meta"]["labels"], a["meta"]; assert a["spec"]["topology"]=="NETWORK_TOPOLOGY_BRIDGE" and a["spec"]["ipv4_cidr"].endswith("/16"), a["spec"]; c=a["conditions"][0]; assert (c["type"],c["status"],c["reason"])==("Realized","CONDITION_STATUS_TRUE","DataplaneRecorded"), c'
  check "o selector de labels filtra, e a paginação dá Link rel=next com um token que continua a lista" ok env \
    SEL_JSON="$(curl -s --unix-socket "$NODESOCK" "$NETS?label_selector=delonix.io%2Fmanaged-by%3Ddelonix")" \
    PG_HDR="$(curl -s -D - -o /dev/null --unix-socket "$NODESOCK" "$NETS?page.page_size=1")" \
    PG_JSON="$(curl -s --unix-socket "$NODESOCK" "$NETS?page.page_size=1")" \
    NODESOCK="$NODESOCK" \
    python3 -c 'import json,os,subprocess; s=json.loads(os.environ["SEL_JSON"]); assert [x["meta"]["name"] for x in s["networks"]]==["e1net-a"], s; p=json.loads(os.environ["PG_JSON"]); assert len(p["networks"])==1 and p["page"]["next_page_token"], p; nxt=[l["href"] for l in p["links"] if l["rel"]=="next"][0]; assert "rel=\"next\"" in os.environ["PG_HDR"], os.environ["PG_HDR"]; q=json.loads(subprocess.run(["curl","-s","--unix-socket",os.environ["NODESOCK"],"http://localhost"+nxt],capture_output=True,text=True).stdout); assert q["networks"] and q["networks"][0]["meta"]["name"]>p["networks"][0]["meta"]["name"], q'
  check "GET de uma rede traz ETag; If-None-Match com esse valor é 304; depois de mudar a rede volta a ser 200" ok env \
    BIN="$BIN" NODESOCK="$NODESOCK" URL="$NETS/e1net-a" \
    python3 -c 'import os,subprocess
def get(*h):
    out=subprocess.run(["curl","-s","-D","-","-o","/dev/null","--unix-socket",os.environ["NODESOCK"],*h,os.environ["URL"]],capture_output=True,text=True).stdout
    lines=out.splitlines(); etag=[l.split(":",1)[1].strip() for l in lines if l.lower().startswith("etag:")]
    return lines[0].split()[1], (etag or [""])[0]
code,etag=get(); assert code=="200" and etag.startswith("\""), (code,etag)
code,same=get("-H","If-None-Match: "+etag); assert (code,same)==("304",etag), (code,same)
subprocess.run([os.environ["BIN"],"network","rm","e1net-a"],capture_output=True); subprocess.run([os.environ["BIN"],"network","create","e1net-a"],capture_output=True)
code,new=get("-H","If-None-Match: "+etag); assert code=="200" and new!=etag, (code,new,etag)'
  check "uma rede que não existe é 404 com um problem+json; noutro namespace a lista é vazia" ok bash -c \
    "[[ \$(curl -s -o /dev/null -w '%{http_code}' --unix-socket '$NODESOCK' '$NETS/nao-existe') == 404 ]] && curl -s --unix-socket '$NODESOCK' http://localhost/v1/namespaces/outro/networks | grep -q '\"networks\":\[\]'"
  e2e_net_rm e1net-a; e2e_net_rm e1net-b
  # ADR-0042 passo E: as mutações de rede pelo contrato, cada uma respondida com
  # uma Operation PERSISTIDA antes do trabalho. Cada cenário lê o que o socket
  # respondeu E o que o motor tem depois (a CLI, os ficheiros do state root).
  NETOPS="$(cd "$(dirname "$0")" && pwd)/e2e_node_network_ops.py"
  netops() { env BIN="$BIN" NODESOCK="$NODESOCK" DELONIX_ROOT="$DELONIX_ROOT" IMG="${IMG:-}" python3 "$NETOPS" "$1"; }
  check "POST …/networks cria a rede: Operation SUCCEEDED com Location, e a CLI e o registo do dataplane têm-na" ok netops create
  check "o mesmo pedido com a mesma Idempotency-Key é respondido com a primeira Operation; sem chave é 409" ok netops replay
  check "a Operation lê-se por id e na lista; a de um processo que já não existe lê-se FAILED/Interrupted" ok netops operations
  check "overlay (501), outro namespace, subnet inválida e um nome com '..' são recusados sem criar nada" ok netops refused
  check "DELETE com If-Match antigo é 412 e a rede fica; com o ETag actual remove-a do motor e do dataplane" ok netops delete
  # ADR-0042 passo E: as leituras de volumes pelo contrato. Um volume criado
  # pela CLI, com dados e uma quota, lê-se pelo socket como o motor o tem.
  E3V="e3vol-$$"
  "$BIN" volume create "$E3V" --quota 1G >/dev/null 2>&1
  printf 'abcde' > "$("$BIN" volume inspect "$E3V" 2>/dev/null | sed -n 's/^ *[Mm]ountpoint: *//p' | head -1)/f" 2>/dev/null
  VOLS="http://localhost/v1/namespaces/default/volumes"
  check "GET …/volumes/{nome} mede o uso, traz a quota e o ETag; a lista diz que não mediu" ok env \
    V_HDR="$(curl -s -D - -o /dev/null --unix-socket "$NODESOCK" "$VOLS/$E3V")" \
    V_JSON="$(curl -s --unix-socket "$NODESOCK" "$VOLS/$E3V")" \
    L_JSON="$(curl -s --unix-socket "$NODESOCK" "$VOLS")" E3V="$E3V" \
    python3 -c 'import json,os; v=json.loads(os.environ["V_JSON"]); n=os.environ["E3V"]; assert v["meta"]["name"]==n and v["meta"]["namespace"]=="default", v["meta"]; assert int(v["used_bytes"])>0, v; assert int(v["spec"]["quota_bytes"])==1<<30 and "local" in v["spec"], v["spec"]; c=v["conditions"][0]; assert (c["type"],c["status"],c["reason"])==("UsageMeasured","CONDITION_STATUS_TRUE","Measured"), c; assert "etag: \""+v["meta"]["etag"]+"\"" in os.environ["V_HDR"].lower(), os.environ["V_HDR"]; l=[x for x in json.loads(os.environ["L_JSON"])["volumes"] if x["meta"]["name"]==n]; assert len(l)==1 and "used_bytes" not in l[0] and l[0]["conditions"][0]["reason"]=="NotMeasuredInList", l'
  check "um volume que não existe é 404 DX-4000; DELETE pelo socket remove o volume do motor (Operation SUCCEEDED, a CLI deixa de o ver)" ok bash -c \
    "[[ \$(curl -s --unix-socket '$NODESOCK' '$VOLS/nao-existe' | python3 -c 'import json,sys; d=json.load(sys.stdin); print(d[\"status\"],d[\"dx\"])') == '404 DX-4000' ]] && [[ \$(curl -s -X DELETE --unix-socket '$NODESOCK' '$VOLS/$E3V' | python3 -c 'import json,sys; print(json.load(sys.stdin)[\"state\"])') == OPERATION_STATE_SUCCEEDED ]] && ! '$BIN' volume inspect '$E3V' >/dev/null 2>&1"
  "$BIN" volume rm "$E3V" >/dev/null 2>&1
  E3N="e3new-$$"
  check "POST …/volumes cria um volume local com quota e labels: Operation SUCCEEDED, e a CLI vê-o com a quota" ok bash -c \
    "[[ \$(curl -s -X POST --unix-socket '$NODESOCK' '$VOLS' -H 'content-type: application/json' -d '{\"name\":\"$E3N\",\"labels\":{\"app\":\"e3\"},\"spec\":{\"quota_bytes\":1073741824}}' | python3 -c 'import json,sys; print(json.load(sys.stdin)[\"state\"])') == OPERATION_STATE_SUCCEEDED ]] && '$BIN' volume inspect '$E3N' | grep -q 1073741824"
  "$BIN" volume rm "$E3N" >/dev/null 2>&1
  # ADR-0042 D3: /docs (Swagger UI) e /redoc (ReDoc), servidos pelo próprio socket
  # a partir de ficheiros embebidos no binário; nada carregado de fora (CSP).
  check "GET /docs e GET /redoc servem HTML com um Content-Security-Policy que só deixa scripts do próprio socket" ok bash -c \
    "for p in /docs /redoc; do h=\$(curl -s -D - -o /dev/null --unix-socket '$NODESOCK' http://localhost\$p); echo \"\$h\" | grep -q '^HTTP/1.1 200' && echo \"\$h\" | grep -qi '^content-type: text/html' && echo \"\$h\" | grep -qi \"^content-security-policy: default-src 'none'; script-src 'self';\" || exit 1; done"
  check "as páginas de docs não carregam nada de fora e cada asset que referem é servido" ok bash -c \
    "for p in /docs /redoc; do b=\$(curl -s --unix-socket '$NODESOCK' http://localhost\$p); echo \"\$b\" | grep -qE 'src=\"(https?:)?//|href=\"(https?:)?//' && exit 1; for a in \$(echo \"\$b\" | grep -oE '/docs/assets/[^\"]+'); do [[ \$(curl -s -o /dev/null -w '%{http_code}' --unix-socket '$NODESOCK' http://localhost\$a) == 200 ]] || exit 1; done; done"
  check "o Swagger UI embebido é o do upstream (sha256 do SHA256SUMS) e o validador online está desligado" ok bash -c \
    "want=\$(grep 'swagger-ui/swagger-ui-bundle.js\$' '$NODE_ASSETS_ROOT/third_party/node-api-docs/SHA256SUMS' | cut -d' ' -f1); got=\$(curl -s --unix-socket '$NODESOCK' http://localhost/docs/assets/swagger-ui-bundle.js | sha256sum | cut -d' ' -f1); [[ -n \$want && \$want == \$got ]] && curl -s --unix-socket '$NODESOCK' http://localhost/docs/assets/swagger-init.js | grep -q 'validatorUrl: null'"
  kill "$NODEPID" 2>/dev/null; wait "$NODEPID" 2>/dev/null; rm -f "$NODESOCK"
else
  skip "node API pelo contrato (ADR-0050 D5)" "sem delonix-node-api ao lado de $BIN (cargo build -p delonix-node-api-bin)"
fi

section "erros: a CLI tem de RECUSAR o que é inválido"
########################################
# Um recurso que não existe afirma-se pela CLASSE (4, «no such …»), nunca por
# `fail` nem por `!`: esses também passam num DX-8101, num root ilegível ou num
# binário que rebenta. Medido a 2026-10-21: container/volume/network/workload/
# secret inspect|describe e gitops status dizem todos 4 sobre um nome inexistente.
check "container describe de inexistente recusa" 4 "$BIN" container describe naoexiste-$PFX
check "container inspect de inexistente recusa" 4 "$BIN" container inspect naoexiste-$PFX
check "volume inspect de inexistente recusa" 4 "$BIN" volume inspect naoexiste-$PFX
check "network inspect de inexistente recusa" 4 "$BIN" network inspect naoexiste-$PFX
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
    "$BIN" vm image build --no-k8s --distro ubuntu --ubuntu-release 24.04 -t e2e-nao-$PFX
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
section "storage: kind StoragePool e volumes num pool (ADR-0067 P0)"
########################################
# The administrator declares a pool in an allowlist; a manifest only NAMES it.
# The pool lives on /dev/shm, not under $OUT: the engine refuses to allocate in
# a pool above 95% full, and the disk a developer runs this on often is — the
# refusal is the engine working, and it would hide every other check here.
SPD="/dev/shm/dlx-e2e-pool-$PFX"; SPF="$OUT/storage-pools-$PFX.yaml"; SPW="$OUT/sp-$PFX"
mkdir -p "$SPW"
sp() { env DELONIX_STORAGE_POOLS_FILE="$SPF" "$BIN" "$@"; }
sp_fns="$(declare -f sp); BIN='$BIN' SPF='$SPF'"
printf 'apiVersion: storage.delonix.io/v1alpha1\nkind: StoragePool\nmetadata: { name: media-%s }\nspec: { alertPct: 85 }\n' "$PFX" >"$SPW/pool.yaml"
printf 'apiVersion: storage.delonix.io/v1alpha1\nkind: StoragePool\nmetadata: { name: media-%s }\nspec: { driver: dir, path: /srv/x }\n' "$PFX" >"$SPW/admin.yaml"
printf 'apiVersion: storage.delonix.io/v1alpha1\nkind: StoragePool\nmetadata: { name: outro-%s }\nspec: {}\n' "$PFX" >"$SPW/other.yaml"
rm -f "$SPF"
check "StoragePool sem allowlist no nó recusa com 69 (DX-6203), nada criado" ok bash -c \
  "$sp_fns; out=\$(sp stack apply -f '$SPW/pool.yaml' 2>&1); rc=\$?; [[ \$rc == 69 ]] && grep -q DX-6203 <<<\"\$out\""
if mkdir -p "$SPD" 2>/dev/null; then
  printf 'pools:\n  media-%s:\n    driver: dir\n    path: %s\n    maxVolumeBytes: 64G\n' "$PFX" "$SPD" >"$SPF"; chmod 644 "$SPF"
  check "um campo do administrador no manifesto (driver, path) é recusado pelo nome: DX-1217, não «unknown field»" ok bash -c \
    "$sp_fns; out=\$(sp stack apply -f '$SPW/admin.yaml' 2>&1); rc=\$?; [[ \$rc == 1 ]] && grep -q DX-1217 <<<\"\$out\" && ! grep -q 'check the spelling' <<<\"\$out\""
  check "um pool que a allowlist não tem é recusado com 77 (DX-7201)" 77 sp stack apply -f "$SPW/other.yaml"
  chmod 666 "$SPF"
  check "uma allowlist que outros podem escrever é recusada (DX-1218)" ok bash -c \
    "$sp_fns; sp stack apply -f '$SPW/pool.yaml' 2>&1 | grep -q DX-1218"
  chmod 644 "$SPF"
  check "stack apply do StoragePool: fica em uso e o plano seguinte não vê deriva" ok bash -c \
    "$sp_fns; sp stack apply -f '$SPW/pool.yaml' >/dev/null 2>&1 && sp get storagepools | grep -q 'media-$PFX .*AVAILABLE' && sp stack plan -f '$SPW/pool.yaml' --detailed-exitcode >/dev/null 2>&1"
  check "volume create --pool --size: os dados ficam DENTRO do pool, com a marca do motor" ok bash -c \
    "$sp_fns; sp volume create 'spv-$PFX' --pool 'media-$PFX' --size 64M >/dev/null 2>&1 && [[ -d '$SPD/spv-$PFX/_data' && -f '$SPD/spv-$PFX/.delonix-volume.json' ]] && sp volume inspect 'spv-$PFX' | grep -q '$SPD/spv-$PFX/_data'"
  check "um volume maior do que o maxVolumeBytes do administrador é recusado com 77" 77 \
    sp volume create "spbig-$PFX" --pool "media-$PFX" --size 100G
  check "um volume que passa o tecto de sobre-alocação do pool é recusado com 5 (DX-5202)" ok bash -c \
    "$sp_fns; out=\$(sp volume create 'spover-$PFX' --pool 'media-$PFX' --size 63G 2>&1); rc=\$?; [[ \$rc == 5 ]] && grep -q DX-5202 <<<\"\$out\" && [[ ! -e '$SPD/spover-$PFX' ]]"
  mkdir -p "$SPD/spalheio-$PFX"; echo dados >"$SPD/spalheio-$PFX/keep"
  check "um directório com dados e sem a marca do motor nunca é adoptado pelo nome (5), e fica intacto" ok bash -c \
    "$sp_fns; sp volume create 'spalheio-$PFX' --pool 'media-$PFX' --size 16M >/dev/null 2>&1; rc=\$?; [[ \$rc == 5 ]] && [[ \$(cat '$SPD/spalheio-$PFX/keep') == dados ]]"
  check "delete storagepools com volumes lá dentro recusa (5) e nomeia-os" ok bash -c \
    "$sp_fns; out=\$(sp delete storagepools 'media-$PFX' 2>&1); rc=\$?; [[ \$rc == 5 ]] && grep -q 'spv-$PFX' <<<\"\$out\""
  check "volume rm liberta o volume do pool; delete deixa de usar o pool e o directório do pool fica" ok bash -c \
    "$sp_fns; sp volume rm 'spv-$PFX' >/dev/null 2>&1 && [[ ! -e '$SPD/spv-$PFX' ]] && sp delete storagepools 'media-$PFX' >/dev/null 2>&1 && [[ -d '$SPD' && -f '$SPD/spalheio-$PFX/keep' ]]"
  printf 'apiVersion: storage.delonix.io/v1alpha1\nkind: StoragePool\nmetadata: { name: media-%s }\nspec: {}\n---\napiVersion: storage.delonix.io/v1alpha1\nkind: Volume\nmetadata: { name: spm-%s }\nspec: { pool: media-%s, size: 32M }\n' "$PFX" "$PFX" "$PFX" >"$SPW/stack.yaml"
  check "por manifesto: pool e volume aplicam, o plano não vê deriva, e o destroy leva o volume, deixa de usar o pool e deixa o directório do pool" ok bash -c \
    "$sp_fns; sp stack apply -f '$SPW/stack.yaml' >/dev/null 2>&1 && [[ -d '$SPD/spm-$PFX/_data' ]] && sp stack plan -f '$SPW/stack.yaml' --detailed-exitcode >/dev/null 2>&1 && sp stack destroy -f '$SPW/stack.yaml' >/dev/null 2>&1 && [[ ! -e '$SPD/spm-$PFX' && -d '$SPD' ]] && sp get storagepools | grep 'media-$PFX ' | grep -qw no"
  rm -rf "$SPD"
else
  skip "storage pools" "não foi possível criar um directório em /dev/shm para o pool"
fi
rm -f "$SPF"

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
if check_then "rota: a 2.ª rede cria-se" ok "$BIN" network create "$NET2" --subnet 10.252.0.0/16 \
   && check_then "rota: network route abre" ok "$BIN" network route "$NET" "$NET2"; then
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
  e2e_net_rm "$NET2"
else
  e2e_net_rm "$NET2"
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
if check_then "network create --gateway dentro do prefixo" ok \
     "$BIN" network create "$NETGW" --subnet 10.251.0.0/16 --gateway 10.251.0.254; then
  # O que se lê no `inspect` tem de ser o que os workloads recebem. O registo
  # declarativo DERIVA sempre `.0.1`; sem ler o plano físico, esta linha dizia
  # um endereço e o dataplane usava outro — a mesma família do nome de bridge
  # que a CLI imprimia sem existir no host.
  check "inspect relata o gateway EM VIGOR, não o derivado" ok bash -c \
    "'$BIN' network inspect '$NETGW' | grep -q '10.251.0.254'"
  check "describe relata o gateway EM VIGOR" ok bash -c \
    "'$BIN' network describe '$NETGW' | grep -q '10.251.0.254'"
  e2e_net_rm "$NETGW"
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

# `--ulimit` is fail-closed (2026-10-11): an unknown name, or a hard limit the
# user cannot raise from a user namespace, used to leave the DEFAULT limit in
# place with rc=0 (measured: `nofile=2000000:2000000` ran with 1048576). The
# init now refuses to start the process, and says which limit and why.
if [[ $E2E_HAVE_IMAGE -eq 1 ]]; then
  check "container run --ulimit applies a limit inside the userns" ok bash -c \
    "'$BIN' container run --rm --net none --ulimit nofile=1234:1234 '$IMG' cat /proc/self/limits | grep -Eq 'open files +1234 +1234'"
  check "container run --ulimit with an unknown name refuses to start" ok bash -c \
    "out=\$('$BIN' container run --rm --net none --ulimit bogus=5 '$IMG' true 2>&1); rc=\$?; [ \$rc -ne 0 ] && grep -q \"'bogus' is not a limit\" <<<\"\$out\""
  _hard=$(ulimit -Hn)
  if [[ "$_hard" =~ ^[0-9]+$ && $(id -u) -ne 0 ]]; then
    check "container run --ulimit above the user's hard limit refuses instead of running without it" ok bash -c \
      "out=\$('$BIN' container run --rm --net none --ulimit nofile=$((_hard + 1)):$((_hard + 1)) '$IMG' true 2>&1); rc=\$?; [ \$rc -ne 0 ] && grep -q 'cannot be raised from a user namespace' <<<\"\$out\""
  fi
fi

# ADR-0048 D4: a declared login is shown by reference, its value only under
# `--reveal`, and the record holds the reference and never the value.
if [[ $E2E_HAVE_IMAGE -eq 1 ]]; then
  CRW="$OUT/cred-$PFX"; mkdir -p "$CRW"; CRC="cred-$PFX"
  cat > "$CRW/m.yaml" <<EOF2
apiVersion: core.delonix.io/v1alpha1
kind: Secret
metadata: {name: $CRC}
spec:
  stringData: {password: v-$PFX}
---
apiVersion: compute.delonix.io/v1alpha1
kind: Container
metadata: {name: $CRC}
spec:
  image: $IMG
  network: none
  command: [sleep, "600"]
  credentials: {user: admin, password: {secret: $CRC, key: password}}
EOF2
  if check_then "credentials: stack apply de um Secret e de um container que o referencia" ok \
       "$BIN" stack apply -f "$CRW/m.yaml"; then
    check "credentials: container ls --credentials shows the user and where the password lives" ok bash -c \
      "'$BIN' container ls --credentials | grep '$CRC' | grep -q 'admin .*(secret $CRC)'"
    check "credentials: --reveal prints the value, and the record does not hold it" ok bash -c \
      "'$BIN' container ls --credentials --reveal | grep '$CRC' | grep -q 'v-$PFX' && ! grep -rqs 'v-$PFX' '$DELONIX_ROOT/containers'"
  fi
  "$BIN" stack destroy -f "$CRW/m.yaml" >/dev/null 2>&1 || true
  # `stack destroy` never removes a Secret (its teardown is off by design).
  "$BIN" secret rm "$CRC" >/dev/null 2>&1 || true
  rm -rf "$CRW"
fi

# ADR-0042 passo E: uma rede com um container ligado não se remove pelo socket
# (409 DX-5307, a nomear o container), e remove-se depois de ele sair. Aqui e
# não na secção do node API porque precisa da imagem.
if [[ $E2E_HAVE_IMAGE -eq 1 && -x "$NODEBIN" ]]; then
  "$BIN" serve node-api --addr "unix://$NODESOCK" >>"$OUT/node-api.log" 2>&1 &
  NODEPID=$!
  for _ in $(seq 100); do [[ -S "$NODESOCK" ]] && break; sleep 0.05; done
  check "DELETE de uma rede em uso é 409 DX-5307 e não escreve Operation; sem o container remove-a" ok \
    env BIN="$BIN" NODESOCK="$NODESOCK" DELONIX_ROOT="$DELONIX_ROOT" IMG="$IMG" \
    python3 "$(cd "$(dirname "$0")" && pwd)/e2e_node_network_ops.py" inuse
  kill "$NODEPID" 2>/dev/null; wait "$NODEPID" 2>/dev/null; rm -f "$NODESOCK"
else
  skip "DELETE de uma rede em uso pelo node API" "sem imagem ou sem delonix-node-api"
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
  check "teardown: the load test's tag leaves the store" ok e2e_image_rm "$TAGX"
  rm -f "$OUT/loadtags.tar"
fi

########################################
section "build: o ENV que a imagem empacota"

# `ENV PATH=/x:$PATH` tem de empacotar o PATH da base com /x à frente. Medido
# antes: a imagem ficava com a cadeia literal `/x:$PATH` (o ENV do último
# estágio era acrescentado duas vezes, a segunda por expandir, e ganha a
# última), e dentro do container `id`/`ls` respondiam «not found».
build_env_path() {
  local d="$OUT/build-env" tag="${PFX}-env:1" got
  mkdir -p "$d"
  printf 'FROM %s\nENV PATH=/opt/tool/bin:$PATH\nCMD ["true"]\n' "$IMG" >"$d/Delonixfile"
  "$BIN" build -t "$tag" "$d" >"$OUT/build-env.log" 2>&1 || { echo "build failed: $(tail -1 "$OUT/build-env.log")"; return 2; }
  # stdout only: the first run of a freshly built image draws its unpacking
  # progress on stderr, and folding that in put the progress line in front of
  # the PATH and failed a check whose PATH was right.
  got=$("$BIN" container run --rm --net none "$tag" sh -c 'command -v ls >/dev/null && echo "$PATH"' 2>"$OUT/build-env.run.err")
  [[ -n "$got" ]] || got="(no PATH printed; stderr: $(tail -2 "$OUT/build-env.run.err"))"
  echo "PATH=$got"
  case "$got" in /opt/tool/bin:/*) return 0 ;; *) return 1 ;; esac
}
check "build: ENV PATH=…:\$PATH empacota o PATH expandido, e as ferramentas da base continuam a resolver" ok build_env_path
check "teardown: the ENV PATH build leaves the store" ok e2e_image_rm "${PFX}-env:1"

# --- `delonix build` passa a ser EXECUTADO sob um check ----------------------
# A bateria já constrói — quatro vezes nesta secção — e o portão contava a
# folha `build` a ZERO. Não é cobertura em falta: é MEDIÇÃO em falta. As
# invocações vivem dentro de funções de shell (`build_env_path`) e de linhas de
# setup (`if "$BIN" build …; then`), e o `cmd` que o `check` grava é o NOME da
# função — o parser do ratchet não atravessa isso, e o doc-comment dele diz que
# o número erra por baixo de propósito.
#
# O check mais barato que corrige a medição é este, e assere o que nenhum
# outro assere: um `-t` dá um nome que o `image ls` mostra, e o `CMD` declarado
# é o que corre. O Delonixfile é escrito FORA do check (uma linha de setup não
# precisa de ser contada) para o corpo ficar sem aspas aninhadas.
_bmin="$OUT/build-min"; _bmt="${PFX}-min:1"
mkdir -p "$_bmin"
printf 'FROM %s\nCMD ["echo","construido-por-e2e"]\n' "$IMG" >"$_bmin/Delonixfile"
check "build: um -t dá um nome que o image ls mostra, e o CMD declarado é o que corre" ok bash -c "
  '$BIN' build -t '$_bmt' '$_bmin' >'$OUT/build-min.log' 2>&1 || { echo \"build chumbou: \$(tail -2 '$OUT/build-min.log')\"; exit 1; }
  '$BIN' image ls 2>&1 | grep -qF '$_bmt' || { echo 'a tag não aparece no image ls'; exit 1; }
  got=\$('$BIN' container run --rm --net none '$_bmt' 2>&1 | tail -1)
  [ \"\$got\" = construido-por-e2e ] || { echo \"o CMD declarado não correu: '\$got'\"; exit 1; }"
check "teardown: the -t build leaves the store" ok e2e_image_rm "$_bmt"
rm -rf "$_bmin"

section "container: ciclo de vida + hot reconfig"
########################################
C="c-$PFX"
# Portas livres e fora do intervalo efémero do host (`e2e_port`, scripts/e2e_ports.sh).
e2e_port P1; e2e_port P2

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

# O USER de uma imagem é o utilizador por omissão (ADR-0062, P2).
#
# Até 2026-10-01 o `container run … id` respondia uid=0 para uma imagem com
# `USER`, desde a v1.0.0. Agora corre como o utilizador que a imagem declara, e
# `-u 0` é a forma explícita de ficar root no container. Num host sem intervalo
# de subuid só cabe um uid: aí continua a 0 e DI-LO. A imagem é construída aqui
# para a bateria não depender de uma imagem com USER estar em cache.
_ud="$OUT/userimg-$PFX"; _ui="e2e-userimg-$PFX:1"
mkdir -p "$_ud" && printf 'FROM %s\nRUN mkdir -p /srv/own && chown 1000:1000 /srv/own\nUSER 1000\n' "$IMG" > "$_ud/Delonixfile"
# Which branch runs is the HOST's answer (`host_has_subuid`), never the run's:
# branching on the `id -u` it printed let a broken USER mapping on a host with
# subuid take the «no subuid» branch.
if check_then "USER da imagem: o build da imagem de teste" ok "$BIN" build -t "$_ui" "$_ud"; then
  _udef="$("$BIN" container run --rm --net none "$_ui" id -u 2>/dev/null)"
  if host_has_subuid; then
    check "USER da imagem: sem -u o processo corre como esse utilizador" ok bash -c \
      "[ '$_udef' = 1000 ] || { echo \"id -u respondeu '$_udef', esperado 1000\"; exit 1; }"
    check "USER da imagem: aplicado, não há aviso" ok bash -c \
      "! '$BIN' container run --rm --net none '$_ui' true 2>&1 >/dev/null | grep -q 'declares USER'"
  else
    check "USER da imagem: sem subuid fica uid 0 e o run diz porquê" ok bash -c \
      "[ '$_udef' = 0 ] && '$BIN' container run --rm --net none '$_ui' true 2>&1 >/dev/null | grep -q 'declares USER 1000'"
  fi
  check "USER da imagem: -u 0 fica root no container" ok bash -c \
    "[ \"\$('$BIN' container run --rm --net none -u 0 '$_ui' id -u 2>/dev/null)\" = 0 ]"
  check "imagem sem USER corre como uid 0, sem aviso" ok bash -c \
    "[ \"\$('$BIN' container run --rm --net none '$IMG' id -u 2>/dev/null)\" = 0 ] && ! '$BIN' container run --rm --net none '$IMG' true 2>&1 >/dev/null | grep -q 'declares USER'"
  check "o /tmp de um container é sticky (1777), não 777" ok bash -c \
    "[ \"\$('$BIN' container run --rm --net none '$IMG' stat -c %a /tmp 2>/dev/null)\" = 1777 ]"
  # O build corre cada RUN como o USER em vigor, e a cache não perde os donos.
  #
  # Medido 2026-10-01 antes da correcção: um RUN depois de `USER app` corria
  # como root; e um rebuild com cache devolvia `/home/app` e uma pasta
  # `chown app` como `root` — o mesmo ficheiro, duas imagens diferentes.
  _bd="$OUT/userbuild-$PFX"; _bi="e2e-userbuild-$PFX"
  mkdir -p "$_bd" && printf 'FROM %s\nRUN adduser -D app\nUSER app\nRUN id -u > /home/app/who\nUSER root\nRUN mkdir /srv/own && chown app:app /srv/own\nUSER app\n' "$IMG" > "$_bd/Delonixfile"
  _bown() { "$BIN" container run --rm --net none "$1" sh -c 'cat /home/app/who; stat -c %U /srv/own /home/app /home/app/who /etc/passwd' 2>/dev/null | tr '\n' ' '; }
  if ! host_has_subuid; then
    skip "build: USER posicional e donos na cache" "este host não dá a $(id -un) um intervalo de subuid (/etc/subuid + newuidmap)"
  elif check_then "build: uma imagem com adduser e USER app" ok "$BIN" build -t "$_bi:1" "$_bd"; then
    check "build: um RUN depois de USER corre como esse utilizador, e os donos ficam na imagem" ok bash -c \
      "[ \"$(_bown "$_bi:1")\" = '1000 app app app root ' ]"
    echo 'RUN true' >> "$_bd/Delonixfile"
    "$BIN" build -t "$_bi:2" "$_bd" >/dev/null 2>&1 || true
    check "build: um rebuild com cache dá os mesmos donos" ok bash -c \
      "[ \"$(_bown "$_bi:2")\" = '1000 app app app root ' ]"
    # Um COPY por cima de um ficheiro que a base dá a um não-root. Medido com
    # `httpd:2.4-alpine` (o `htdocs/index.html` é do uid 501): desde que a árvore
    # de trabalho leva os donos da imagem, o COPY respondia `Permission denied`.
    printf 'FROM %s\nCOPY who /home/app/who\n' "$_bi:1" > "$_bd/Delonixfile"; echo copied > "$_bd/who"
    "$BIN" build -t "$_bi:3" "$_bd" >/dev/null 2>&1 || true
    check "build: um COPY substitui um ficheiro que a base dá a um não-root, e o novo é de root" ok bash -c \
      "[ \"\$('$BIN' container run --rm --net none '$_bi:3' sh -c 'cat /home/app/who; stat -c %U /home/app/who' 2>/dev/null | tr '\\n' ' ')\" = 'copied root ' ]"
    check "teardown: the USER builds leave the store" ok e2e_image_rm "$_bi:1" "$_bi:2" "$_bi:3"
  fi
  rm -rf "$_bd"

  # Um utilizador não-root recebe o que a IMAGEM lhe dá, e mais nada (ADR-0062 P1).
  #
  # Medido 2026-10-01 antes da correcção, com `-u`: o init fazia `chown -R` ao
  # rootfs inteiro — 940 de 986 entradas do utilizador, que escreveu em
  # /etc/passwd e substituiu o seu próprio binário — e a varredura atravessava
  # os mounts: uma pasta do HOST montada por bind passou de 1000:1000 para um
  # subuid, e o dono deixou de conseguir escrever no seu ficheiro. Cada check
  # abaixo é um desses factos, lido de dentro do container ou no host.
  _ub="$OUT/userbind-$PFX"; _uv="e2e-uservol-$PFX"
  mkdir -p "$_ub" && echo dono > "$_ub/f"
  _own_before="$(stat -c %u:%g "$_ub" "$_ub/f" | tr '\n' ' ')"
  "$BIN" volume create "$_uv" >/dev/null 2>&1 || true
  _urun() { "$BIN" container run --rm --net none -u 1000 -v "$_ub:/bind" -v "$_uv:/vol" "$_ui" sh -c "$1" 2>/dev/null; }
  export -f _urun; export BIN _ub _uv _ui
  if ! host_has_subuid; then
    skip "-u: posse do rootfs e dos mounts" "este host não dá a $(id -un) um intervalo de subuid (/etc/subuid + newuidmap)"
  elif check_then "-u 1000 com um bind e um volume corre como 1000" ok bash -c \
         "[ \"\$(_urun 'id -u')\" = 1000 ]"; then
    check "-u: o utilizador NÃO escreve em /etc/passwd" fail bash -c "_urun 'echo x >> /etc/passwd'"
    check "-u: /etc/passwd continua a ser de root" ok bash -c "[ \"\$(_urun 'stat -c %u /etc/passwd')\" = 0 ]"
    check "-u: a pasta que a imagem lhe dá é dele e é gravável" ok bash -c \
      "[ \"\$(_urun 'stat -c %u:%g /srv/own && touch /srv/own/x && echo ok' | tr '\n' ' ')\" = '1000:1000 ok ' ]"
    check "-u: um volume nomeado vazio é gravável pelo utilizador" ok bash -c "_urun 'touch /vol/x'"
    check "-u: o bind mount do HOST mantém o dono (não é re-apropriado)" ok bash -c \
      "[ \"\$(stat -c %u:%g '$_ub' '$_ub/f' | tr '\n' ' ')\" = '$_own_before' ]"
    check "-u: o dono do bind continua a escrever no seu ficheiro" ok bash -c "echo mais >> '$_ub/f'"
    # Um `container commit` empacota do lado do HOST. Gravava os números do host
    # nos cabeçalhos: medido, todas as entradas 1000:1000 (quem invoca) e a pasta
    # do uid 1000 do container como 100999 (o subuid) — uma imagem que dá o rootfs
    # inteiro ao uid 1000 em qualquer motor que respeite a posse do tar.
    _uc="e2e-usercommit-$PFX"; _uci="e2e-usercommit-$PFX:1"
    if check_then "commit: o container de teste arranca (-u 1000)" ok \
         "$BIN" container run -d --name "$_uc" --net none -u 1000 "$_ui" sleep 120; then
      check "container commit" ok "$BIN" container commit "$_uc" "$_uci"
      check "commit: a layer guarda os donos que o CONTAINER vê" ok bash -c \
        "[ \"\$('$BIN' container run --rm --net none -u 1000 '$_uci' stat -c %u /etc/passwd /srv/own 2>/dev/null | tr '\n' ' ')\" = '0 1000 ' ]"
      check "commit: o USER da imagem base sobrevive" ok bash -c \
        "[ \"\$('$BIN' container run --rm --net none '$_uci' id -u 2>/dev/null)\" = 1000 ]"
    fi
    e2e_rm "$_uc"
    check "teardown: the committed image leaves the store" ok e2e_image_rm "$_uci"
  fi
  "$BIN" container run --rm --net none -u 0 -v "$OUT:/o" "$_ui" rm -rf "/o/userbind-$PFX" >/dev/null 2>&1 || true
  "$BIN" volume rm "$_uv" >/dev/null 2>&1 || true
  rm -rf "$_ub"
  check "teardown: the USER test image leaves the store" ok e2e_image_rm "$_ui"
fi
rm -rf "$_ud"

check "container run -d -p" ok "$BIN" container run -d --name "$C" -p "$P1:80" "$IMG" sleep 600
if "$BIN" container inspect "$C" >/dev/null 2>&1; then
  check "container ls mostra-o" ok bash -c "'$BIN' container ls | grep -q '$C'"
  check "container describe" ok "$BIN" container describe "$C"
  check "container inspect (JSON válido)" ok bash -c "'$BIN' container inspect '$C' | python3 -m json.tool >/dev/null"
  check "container exec" ok "$BIN" container exec "$C" /bin/true
  # A command that does not exist: 127 AND the reason. Measured before: 127
  # with nothing on stderr, which reads as a command that ran and printed nothing.
  check "exec de um comando inexistente sai 127" 127 "$BIN" container exec "$C" no-such-command-e2e
  check "exec de um comando inexistente diz qual e porquê" ok sh -c \
    "'$BIN' container exec '$C' no-such-command-e2e 2>&1 | grep -q 'exec no-such-command-e2e'"
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
e2e_rm "rc-$PFX"

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
e2e_rm "rb-$PFX"
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
e2e_rm "rs-$PFX"
sleep 2
check "e o rm -f não deixa nenhuma encarnação para trás" ok bash -c "[ \"\$(pgrep -f -x 'sleep $RS_SLEEP' | wc -l)\" = 0 ]"

# O `rename` simples, sem corrida de tempos. Até 2026-10-21 a folha só era
# executada pelo check da espera entre reinícios, logo abaixo, que salta quando
# não apanha o container parado — e cada corrida que não o apanhava descia o
# ratchet de execução de 266 para 265 sem nada ter mudado no motor. Uma folha
# não pode depender de uma janela de tempo para ser executada.
if check_then "container rename: o container de prova arranca" ok \
     "$BIN" container run -d --net none --name "rnp-$PFX" "$IMG" sleep 600; then
  check "container rename" ok "$BIN" container rename "rnp-$PFX" "rnp2-$PFX"
  # The old name must be GONE (class 4, DX-4101), not merely unreadable: a bare
  # `!` would also pass on a DX-8101 or an unreadable root.
  check "container rename: o nome novo responde e o antigo já não existe" ok bash -c \
    "'$BIN' container inspect 'rnp2-$PFX' >/dev/null || exit 1
     '$BIN' container inspect 'rnp-$PFX' >/dev/null 2>&1; [ \$? -eq 4 ]"
fi
e2e_rm "rnp-$PFX" "rnp2-$PFX"

# Um `rename` feito na espera entre reinícios fica. O supervisor arrancava da
# cópia do registo que trazia desde o primeiro arranque, e o `create_with` grava o
# registo inteiro: o reinício seguinte repunha o nome antigo.
"$BIN" container run -d --net none --restart always --name "rn-$PFX" "$IMG" sh -c 'sleep 1; exit 1' >/dev/null 2>&1
if e2e_in_backoff "rn-$PFX" 0; then
  RN0=$(e2e_restarts "rn-$PFX")
  check "container rename" ok "$BIN" container rename "rn-$PFX" "rn2-$PFX"
  check "um rename na espera entre reinícios sobrevive ao reinício" ok bash -c \
    "for _ in \$(seq 1 40); do n=\$('$BIN' container ls -a -o json | python3 -c \"import json,sys; print(next((c.get('restarts') for c in json.load(sys.stdin) if c.get('name')=='rn2-$PFX'),-1))\"); [ \"\$n\" -gt '$RN0' ] 2>/dev/null && exit 0; sleep 0.5; done; exit 1"
else
  skip "um rename na espera entre reinícios sobrevive ao reinício" "não apanhei o container entre reinícios"
fi
e2e_rm "rn-$PFX" "rn2-$PFX"

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
e2e_rm "ss-$PFX"
sleep 1
check "e o rm -f não deixa o processo para trás" ok bash -c "[ \"\$(pgrep -f -x 'sleep $SS_SLEEP' | wc -l)\" = 0 ]"
pkill -9 -f -x "sleep $SS_SLEEP" 2>/dev/null || true

# O mesmo, do lado do `stop`: um `stop` à espera (PID 1 sem handler de SIGTERM)
# enquanto o processo morre por fora e um `start` grava o processo novo. Ao
# retomar, o `stop` gravava `pid = None` por cima dele, e o processo novo
# sobrevivia ao `rm -f`. Congelar o `stop` (SIGSTOP) torna a corrida determinística.
SW_SLEEP=$(( 5000 + RANDOM % 900 ))
SW_X=
if check_then "stop/start: o container de teste arranca" ok \
     "$BIN" container run -d --net none --name "sw-$PFX" "$IMG" sh -c "exec sleep $SW_SLEEP" \
   && sleep 1 && SW_X=$(pgrep -f -x "sleep $SW_SLEEP" | head -1) \
   && check_then "stop/start: o processo do container está no host" ok test -n "$SW_X"; then
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
fi
e2e_rm "sw-$PFX"
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
section "network route: uma carga ligada por rede ADICIONAL usa a rota (ADR-0013 tier B)"
########################################
# A route is one `iifname A . oifname B` pair, so it only serves a packet that
# ENTERS by A's bridge. A workload attached to A as an additional network kept
# sending B's traffic out of its default route, and the route carried nothing
# while reporting itself open (measured 2026-10-08: 100 % loss both ways with
# the pair installed). The holder now installs, in each such workload, the route
# to the other network through this network's gateway — and the mirror on the
# far end, so replies come back the way they went.
MH_BASE="mhb-$PFX"; MH_A="mha-$PFX"; MH_B="mhz-$PFX"
MH_WA="mhwa-$PFX"; MH_WB="mhwb-$PFX"; MH_WC="mhwc-$PFX"
mh_ip() { "$BIN" container exec "$1" ip -4 -o addr show eth1 2>/dev/null | awk '{print $4}' | cut -d/ -f1; }
if check_then "multi-homed: a rede base cria-se" ok "$BIN" network create "$MH_BASE" \
   && check_then "multi-homed: a rede A cria-se" ok "$BIN" network create "$MH_A" \
   && check_then "multi-homed: a rede B cria-se" ok "$BIN" network create "$MH_B" \
   && check_then "multi-homed: a carga A arranca na rede base" ok \
        "$BIN" container run -d --name "$MH_WA" --net "$MH_BASE" "$IMG" sleep 600 \
   && check_then "multi-homed: a carga B arranca na rede base" ok \
        "$BIN" container run -d --name "$MH_WB" --net "$MH_BASE" "$IMG" sleep 600 \
   && check_then "multi-homed: network connect A a quente" ok "$BIN" network connect "$MH_A" "$MH_WA" \
   && check_then "multi-homed: network connect B a quente" ok "$BIN" network connect "$MH_B" "$MH_WB"; then
  MH_IPA="$(mh_ip "$MH_WA")"; MH_IPB="$(mh_ip "$MH_WB")"
  check "multi-homed: as duas cargas têm endereço na rede adicional" ok test -n "$MH_IPA" -a -n "$MH_IPB"
  check "multi-homed: sem rota, A não alcança B" fail "$BIN" container exec "$MH_WA" ping -c 2 -W 1 "$MH_IPB"
  "$BIN" network route "$MH_A" "$MH_B" >/dev/null 2>&1
  check "multi-homed: com a rota A→B, a carga em A alcança a carga em B" ok "$BIN" container exec "$MH_WA" ping -c 2 -W 1 "$MH_IPB"
  check "multi-homed: a rota é dirigida — B não abre conversa para A" fail "$BIN" container exec "$MH_WB" ping -c 2 -W 1 "$MH_IPA"
  check "multi-homed: a carga em A tem a rota para a rede B pela eth1" ok bash -c \
    "'$BIN' container exec '$MH_WA' ip route | grep -q ' via .* dev eth1'"
  if check_then "multi-homed: a carga C arranca depois da rota" ok \
       "$BIN" container run -d --name "$MH_WC" --net "$MH_BASE" "$IMG" sleep 600 \
     && check_then "multi-homed: network connect C a quente" ok "$BIN" network connect "$MH_A" "$MH_WC"; then
    check "multi-homed: uma carga ligada DEPOIS de a rota existir também a usa" ok "$BIN" container exec "$MH_WC" ping -c 2 -W 1 "$MH_IPB"
  fi
  "$BIN" network route "$MH_A" "$MH_B" --rm >/dev/null 2>&1
  check "multi-homed: rota fechada, a carga deixa de alcançar B" fail "$BIN" container exec "$MH_WA" ping -c 2 -W 1 "$MH_IPB"
  check "multi-homed: rota fechada, a rota sai da tabela da carga" ok bash -c \
    "! '$BIN' container exec '$MH_WA' ip route | grep -q ' via .* dev eth1'"
fi
"$BIN" network route "$MH_A" "$MH_B" --rm >/dev/null 2>&1 || true
for c in "$MH_WA" "$MH_WB" "$MH_WC"; do e2e_rm "$c"; done
for n in "$MH_A" "$MH_B" "$MH_BASE"; do e2e_net_rm "$n"; done

########################################
section "container em rede custom: dois starts concorrentes (#798)"
########################################
# Dois `container start` do mesmo container parado passavam os dois a verificação
# «já está a correr»; o que perdia desfazia depois a rede do que ganhou, e o
# container ficava a correr sem `eth0` — medido na v5.0.2, 5 em 5 rondas. Só
# conta uma ronda em que exactamente um start ganhou: com o `stop` ainda a acabar
# os dois recusam, e essa ronda não diz nada sobre a corrida.
CR="cr-$PFX"
if check_then "start concorrente: o container arranca na rede custom" ok \
     "$BIN" container run -d --name "$CR" --net "$NET" "$IMG" sleep 600; then
  CR_ROUNDS=0 CR_LOST=0
  for _r in 1 2 3 4 5 6; do
    # Com o disco do host ocupado, o PID 1 fica em D à espera do writeback e o
    # `stop` desiste aos 30 s (DX-8101) com o container ainda registado a correr
    # (ADR-0056 D4). Repete-se até ele sair; uma ronda que não chega lá não conta.
    # The state is read from the record. This used to say `container ps` lists
    # stopped containers too; true when written, stale since #816 (without -a it
    # lists only Running and Paused, as `docker ps` does). The record stays the
    # better question: it names the status, where absence from `ps` would not
    # tell a finished stop from a vanished record.
    _gone=0
    for _i in 1 2 3 4 5 6; do
      "$BIN" container stop -t 0 "$CR" >/dev/null 2>&1
      "$BIN" container inspect "$CR" 2>/dev/null | grep -qE '"status": *"Running"' \
        || { _gone=1; break; }
    done
    [[ $_gone -eq 1 ]] || continue
    "$BIN" container start "$CR" >/dev/null 2>&1 & _p1=$!
    "$BIN" container start "$CR" >/dev/null 2>&1 & _p2=$!
    wait $_p1; _r1=$?; wait $_p2; _r2=$?
    [[ $((_r1 == 0)) -ne $((_r2 == 0)) ]] || continue
    CR_ROUNDS=$((CR_ROUNDS+1))
    "$BIN" container exec "$CR" ip -4 -o addr show eth0 2>/dev/null | grep -q inet \
      || CR_LOST=$((CR_LOST+1))
  done
  if [[ $CR_ROUNDS -gt 0 ]]; then
    check "start concorrente: o container que ganhou mantém a rede" ok \
      bash -c "echo 'rondas=$CR_ROUNDS sem_eth0=$CR_LOST'; test $CR_LOST -eq 0"
  else
    skip "start concorrente" "nenhuma ronda teve exactamente um start vencedor"
  fi
  e2e_rm "$CR"
fi

########################################
section "container em rede custom: hot reconfig pelo ingress"
########################################
CN="cn-$PFX"
NET2="net2-$PFX"
if check_then "hot reconfig: a rede extra cria-se" ok "$BIN" network create "$NET2" --subnet 10.252.0.0/16 \
   && check_then "hot reconfig: o container arranca na rede custom" ok \
        "$BIN" container run -d --name "$CN" --net "$NET" "$IMG" sleep 600; then
  check "network connect a quente" ok "$BIN" network connect "$NET2" "$CN"
  check "network connect: rede extra no describe" ok bash -c "'$BIN' container describe '$CN' | grep -q '$NET2'"
  check "network connect repetido recusa" fail "$BIN" network connect "$NET2" "$CN"
  check "update: net-rate a quente" ok "$BIN" container update "$CN" --net-rate 10mbit
  check "update: taxa inválida recusa" fail "$BIN" container update "$CN" --net-rate depressa
  check "update: net-rate-clear" ok "$BIN" container update "$CN" --net-rate-clear
  check "network disconnect a quente" ok "$BIN" network disconnect "$NET2" "$CN"
  check "network disconnect de rede não ligada recusa" fail "$BIN" network disconnect "$NET2" "$CN"
  check "container update --net-connect deixou de existir (corte limpo)" 2 "$BIN" container update "$CN" --net-connect "$NET2"
  e2e_rm "$CN"
  # `--net-bps` no `run -d` era aceite e ignorado: o registo dizia a taxa e o veth
  # não tinha qdisc nenhuma (o shaping só corria depois do retorno do supervisor).
  # Prova-se no dataplane, dentro do holder — o registo é exactamente o que mentia.
  CB="cb-$PFX"
  if "$BIN" container run -d --name "$CB" --net "$NET" --net-bps 1mbit "$IMG" sleep 120 >/dev/null 2>&1; then
    PIN=$("$BIN" net netns status 2>/dev/null | grep -oE 'pin [0-9]+' | grep -oE '[0-9]+')
    check "run -d --net-bps aplica o shaping no veth" ok bash -c \
      "nsenter -t '$PIN' -U -n --preserve-credentials tc qdisc show | grep -q 'tbf .*rate 1Mbit'"
    e2e_rm "$CB"
  else
    check "run -d --net-bps numa rede custom" ok false
  fi
fi
e2e_net_rm "$NET2"

########################################
section "publish: o endereço de bind é REGISTADO e sobrevive a um start"
########################################
# O endereço com que uma porta publicada liga era estado USADO na publicação e
# nunca PERSISTIDO: o registo guardava `51072:80` e o `DELONIX_PUBLISH_ADDR` era
# relido do ambiente a cada `start`. Medido a 2026-10-07, nos dois sentidos — uma
# porta exposta à LAN voltava em 127.0.0.1 depois de um `start` (que é o que um
# unit do `net boot enable` corre, sem ambiente nenhum), e uma porta publicada de
# propósito só em loopback voltava em 0.0.0.0 quando a shell do operador tinha a
# variável exportada. Nos dois casos rc=0, sem aviso, com o `ingress ls` a
# imprimir a linha igual antes e depois.
#
# O veredicto é o bind lido do KERNEL (`ss`), nunca o que o comando disse — e tem
# de ser através de um `stop`+`start`, porque cada passo isolado devolve 0 mesmo
# com o defeito. Um check pelo rc ficaria verde sobre o bug.
CPB="cpb-$PFX"
# Portas livres e fora do intervalo efémero (`e2e_port`); se alguma for ocupada
# entretanto o troço salta com razão audível em vez de chumbar por causa de um
# vizinho. (Eram 51081–51083 fixas, dentro do intervalo efémero por omissão.)
e2e_port P_LOOP; e2e_port P_SPEC; e2e_port P_ENVV
if ss -tulnH 2>/dev/null | grep -qE ":($P_LOOP|$P_SPEC|$P_ENVV) "; then
  skip "endereço de bind persistido" "uma das portas $P_LOOP/$P_SPEC/$P_ENVV já está ocupada neste host"
elif ! check_then "endereço de bind: o container arranca na rede custom" ok \
       "$BIN" container run -d --name "$CPB" --net "$NET" "$IMG" sleep 600; then
  :  # já é um FAIL, com o erro; os checks abaixo dependem dele
else
  # 1) Sem endereço: liga só ao loopback, e o REGISTO di-lo (é o que torna o
  #    `start` seguinte determinístico em vez de dependente do ambiente).
  check "publish sem endereço liga em 127.0.0.1" ok bash -c \
    "'$BIN' net ingress publish '$CPB' $P_LOOP:80 >/dev/null && ss -tlnH | grep -q '127.0.0.1:$P_LOOP '"
  check "publish sem endereço NÃO liga em 0.0.0.0" ok bash -c \
    "! ss -tlnH | grep -qE '(^|[^.0-9])0\\.0\\.0\\.0:$P_LOOP '"
  check "o registo guarda o endereço resolvido" ok bash -c \
    "'$BIN' net ingress ls '$CPB' | grep -q '127.0.0.1:$P_LOOP:80'"

  # 2) Endereço no spec, em UDP — a forma que o `--help` não documentava e que
  #    era a razão de alguém concluir que só havia loopback.
  check "publish com endereço no spec liga em 0.0.0.0 (udp)" ok bash -c \
    "'$BIN' net ingress publish '$CPB' 0.0.0.0:$P_SPEC:80/udp >/dev/null && ss -ulnH | grep -q '0.0.0.0:$P_SPEC '"

  # 3) A via do env var: o endereço tem de ficar REGISTADO, senão desaparece.
  check "publish via DELONIX_PUBLISH_ADDR liga em 0.0.0.0" ok bash -c \
    "DELONIX_PUBLISH_ADDR=0.0.0.0 '$BIN' net ingress publish '$CPB' $P_ENVV:80 >/dev/null && ss -tlnH | grep -q '0.0.0.0:$P_ENVV '"
  check "o endereço do env var fica no registo, não só no dataplane" ok bash -c \
    "'$BIN' net ingress ls '$CPB' | grep -q '0.0.0.0:$P_ENVV:80'"

  # O CICLO. Sem ambiente nenhum, como um unit de boot.
  "$BIN" container stop "$CPB" >/dev/null 2>&1
  sleep 1
  if "$BIN" container start "$CPB" >/dev/null 2>&1; then
    sleep 2
    # O que o bug fazia: 0.0.0.0 → 127.0.0.1, em silêncio.
    check "start: a porta exposta pelo spec CONTINUA em 0.0.0.0" ok bash -c \
      "ss -ulnH | grep -q '0.0.0.0:$P_SPEC '"
    check "start: a porta exposta pelo env var CONTINUA em 0.0.0.0" ok bash -c \
      "ss -tlnH | grep -q '0.0.0.0:$P_ENVV '"
    check "start: a porta de loopback CONTINUA em 127.0.0.1" ok bash -c \
      "ss -tlnH | grep -q '127.0.0.1:$P_LOOP '"
  else
    check "start de um container com portas publicadas" ok false
  fi

  # O sentido INVERSO, que é o que pesa em segurança: um `start` corrido numa
  # shell com a variável exportada não pode expor à LAN uma porta que foi
  # publicada de propósito só em loopback.
  "$BIN" container stop "$CPB" >/dev/null 2>&1
  sleep 1
  if DELONIX_PUBLISH_ADDR=0.0.0.0 "$BIN" container start "$CPB" >/dev/null 2>&1; then
    sleep 2
    check "start com o env var NÃO expõe uma porta publicada em loopback" ok bash -c \
      "ss -tlnH | grep -q '127.0.0.1:$P_LOOP ' && ! ss -tlnH | grep -qE '(^|[^.0-9])0\\.0\\.0\\.0:$P_LOOP '"
  else
    check "start com o env var exportado" ok false
  fi
  e2e_rm "$CPB"
fi

########################################
section "network: IPAM sem leases órfãos (S2, doc 62 §6 P1)"
########################################
# O registo de endereços (`network ipam ls`) ANTES e DEPOIS de cada caminho de
# saída tem de ter o MESMO número de leases órfãos — zero novos. Medido contra a
# v4.4.0+68: numa rede criada com `--subnet 10.X.0.0/16` o lease era gravado em
# `10.X.0.0_16` e libertado em `10.X`, por isso `rm -f` e `network rm` deixavam
# DOIS órfãos por container multi-homed (a rede primária e a extra). E o `stop`
# que liberta o lease (outra metade do mesmo achado) mudava o IP no `start`.
#
# Conta-se pelo JSON e não pela tabela: a coluna OWNER diz `<orphaned>` para um
# id sem registo, e é exactamente isso que se quer contar.
ipam_orfaos() {
  "$BIN" network ipam ls -o json 2>/dev/null | python3 -c \
    'import json,sys; print(sum(1 for r in json.load(sys.stdin) if r["owner"]=="<orphaned>"))'
}
# Os leases de um container, pelo NOME do dono (o `ls` resolve id → nome).
ipam_de() {
  "$BIN" network ipam ls -o json 2>/dev/null | python3 -c \
    'import json,sys; print(" ".join(sorted(r["ip"] for r in json.load(sys.stdin) if r["owner"]==sys.argv[1])))' "$1"
}
# A rede PRIMÁRIA é criada SEM `--subnet` (chave legada `10.X`) e a extra COM
# ele: é na primária que o `stop` libertava o lease na chave CERTA — numa rede
# CIDR o release ia para a chave errada e o lease «ficava» por acidente, e o
# check do stop passava contra o defeito.
LC="lc-$PFX"; NET3="net3-$PFX"; NET4="net4-$PFX"
ORF0=$(ipam_orfaos)
if check_then "ipam: network ipam ls -o json conta os órfãos" ok test -n "$ORF0" \
   && check_then "ipam: a rede CIDR cria-se" ok "$BIN" network create "$NET3" --subnet 10.250.0.0/16 \
   && check_then "ipam: a rede legada cria-se" ok "$BIN" network create "$NET4" \
   && check_then "ipam: o container arranca na rede legada" ok \
        "$BIN" container run -d --name "$LC" --net "$NET4" "$IMG" sleep 600 \
   && check_then "ipam: network connect à rede CIDR" ok "$BIN" network connect "$NET3" "$LC"; then
  IPS0=$(ipam_de "$LC")
  check "ipam: um container multi-homed tem um lease por rede" ok bash -c \
    "[ \"\$(wc -w <<<'$IPS0')\" = 2 ]"
  "$BIN" container stop "$LC" >/dev/null 2>&1
  # A expansão `$(ipam_de ...)` corre AQUI, depois do stop — o `bash -c` só
  # compara; é o mesmo padrão dos checks abaixo.
  check "ipam: o stop guarda os leases (as duas redes)" ok bash -c "[ \"$(ipam_de "$LC")\" = '$IPS0' ]"
  "$BIN" container start "$LC" >/dev/null 2>&1
  check "ipam: o start volta aos MESMOS endereços" ok bash -c "[ \"$(ipam_de "$LC")\" = '$IPS0' ]"
  e2e_rm "$LC"
  check "ipam: rm -f não deixa lease nenhum do container" ok bash -c "[ -z \"$(ipam_de "$LC")\" ]"
  check "ipam: rm -f não deixa leases órfãos (antes $ORF0)" ok bash -c "[ \"$(ipam_orfaos)\" = '$ORF0' ]"
  e2e_net_rm "$NET3"
  e2e_net_rm "$NET4"
  check "ipam: network rm não deixa leases órfãos (antes $ORF0)" ok bash -c "[ \"$(ipam_orfaos)\" = '$ORF0' ]"
else
  e2e_rm "$LC"
  e2e_net_rm "$NET3"
  e2e_net_rm "$NET4"
fi
# O mesmo pelo caminho declarativo: uma rede com CIDR e um container nela, e o
# `stack destroy` tem de levar os leases com eles.
IWORK="$OUT/ipam-$PFX"; mkdir -p "$IWORK"
cat >"$IWORK/m.yaml" <<YAML
apiVersion: delonix.io/v1
kind: Network
metadata:
  name: in-$PFX
spec:
  driver: bridge
  subnet: 10.249.0.0/16
---
apiVersion: compute.delonix.io/v1alpha1
kind: Container
metadata:
  name: ic-$PFX
spec:
  image: $IMG
  command: ["sleep", "600"]
  network: in-$PFX
YAML
ORF0=$(ipam_orfaos)
if check_then "ipam: stack apply de uma rede CIDR com um container" ok "$BIN" stack apply -f "$IWORK/m.yaml" \
   && check_then "ipam: o container do apply tem lease" ok bash -c "[ -n \"$(ipam_de "ic-$PFX")\" ]"; then
  "$BIN" stack destroy -f "$IWORK/m.yaml" >/dev/null 2>&1
  check "ipam: stack destroy não deixa leases órfãos (antes $ORF0)" ok bash -c "[ \"$(ipam_orfaos)\" = '$ORF0' ]"
  check "ipam: nenhum lease fica na rede destruída" ok bash -c \
    "! '$BIN' network ipam ls -o json | grep -q '10\\.249\\.'"
else
  "$BIN" stack destroy -f "$IWORK/m.yaml" >/dev/null 2>&1
fi

########################################

section "network: isolamento por namespace sem br_netfilter é RECUSADO (D5)"
########################################
# Decision D5 of the maturity plan: a named namespace on a host that does not
# filter bridge traffic is REFUSED (DX-6305, exit 69). Before it was a warning
# and `run` exited 0 on a tenant boundary that blocked nothing.
#
# The host here HAS br_netfilter, so the condition is made where it lives: the
# sysctl is per network namespace, and it is set to 0 INSIDE this run's
# isolated holder only. A keeper container holds the holder up, because a
# refused attach that left the holder empty tears it down, and the next one is
# born with the default (1) — measured, that is how the valve check first
# passed for the wrong reason.
#
# Never on shared state: there the holder is the real one, and the sysctl would
# switch off isolation for every workload on the node.
REAL_ROOT="${XDG_DATA_HOME:-$HOME/.local/share}/delonix"
if [[ "$E2E_ISOLATED" != 1 || "$DELONIX_ROOT" == "$REAL_ROOT" ]]; then
  skip "D5: recusa sem br_netfilter" "estado partilhado — o sysctl tocaria no holder real"
else
  N5="n5-$PFX"; K5="k5-$PFX"
  if check_then "D5: a rede cria-se" ok "$BIN" network create "$N5" \
     && check_then "D5: o container que segura o holder arranca" ok \
          "$BIN" container run -d --name "$K5" --net "$N5" "$IMG" sleep 600 \
     && check_then "D5: o sysctl bridge-nf-call-iptables vai a 0 no holder ISOLADO" ok bash -c \
          "pin=\$(cat '$DELONIX_ROOT/ingress/holder.pid') && nsenter -t \"\$pin\" -U -n --preserve-credentials sh -c 'echo 0 > /proc/sys/net/bridge/bridge-nf-call-iptables'"; then
    ORF5=$(ipam_orfaos)
    check "D5: --namespace sem filtragem de bridge sai com 69" 69 \
      "$BIN" container run -d --name "r5-$PFX" --namespace teama --net "$N5" "$IMG" sleep 600
    check "D5: e diz DX-6305" ok bash -c \
      "'$BIN' container run -d --name r5b-$PFX --namespace teama --net '$N5' '$IMG' sleep 600 2>&1 | grep -q DX-6305"
    check "D5: o recusado não fica no store" ok bash -c \
      "! '$BIN' container ps -a | grep -q ' r5-$PFX '"
    check "D5: nem deixa lease órfão" ok test "$(ipam_orfaos)" = "$ORF5"
    check "D5: um pod numa namespace também é recusado" 69 bash -c \
      "printf 'apiVersion: compute.delonix.io/v1alpha1\nkind: Pod\nmetadata: {name: p5-$PFX, namespace: teamx}\nspec:\n  network: $N5\n  containers:\n  - {name: a, image: $IMG, command: [sleep, \"600\"]}\n' > '$OUT/p5.yaml' && '$BIN' pod create -f '$OUT/p5.yaml'"
    check "D5: a válvula avisa e deixa correr" ok bash -c \
      "DELONIX_ALLOW_UNENFORCED_ISOLATION=1 '$BIN' container run -d --name v5-$PFX --namespace teama --net '$N5' '$IMG' sleep 600 2>&1 | grep -q 'would not be enforced'"
    check "D5: a namespace default nunca é recusada" ok \
      "$BIN" container run -d --name d5-$PFX --net "$N5" "$IMG" sleep 600
  fi
  for c in "r5-$PFX" "r5b-$PFX" "v5-$PFX" "d5-$PFX" "$K5" "p5-$PFX-a"; do
    e2e_rm "$c"
  done
  "$BIN" pod rm "p5-$PFX" >/dev/null 2>&1
  e2e_net_rm "$N5"
fi
section "network: anti-spoofing na bridge — origem e MAC forjados são bloqueados"
########################################
# Until 2026-10-02 the anti-spoof rule lived in `table ip` and never matched:
# with br_netfilter the IP layer sees the BRIDGE as the input interface, not
# the port. Measured against the engine, a container with NET_ADMIN forged its
# source, reached its neighbour (3/3) and crossed namespace isolation by
# forging a member of the other namespace (0 packets with its own address, 3
# with the forged one). The table is now `bridge dlxspoof`, at prerouting.
#
# Every check counts echo requests RECEIVED by the target (Icmp InEchos), not
# the sender's rc: a ping whose reply is dropped also fails, and a check on the
# rc would pass on a defect that delivers the forged packet.
AS="as-$PFX"; A1="as1-$PFX"; A2="as2-$PFX"; A3="as3-$PFX"; A4="as4-$PFX"
echos_of() { "$BIN" container exec "$1" awk '/^Icmp:/{getline; print $10}' /proc/net/snmp 2>/dev/null; }
ip_of() { "$BIN" container inspect "$1" 2>/dev/null | python3 -c 'import json,sys;d=json.load(sys.stdin);d=d[0] if isinstance(d,list) else d;print(d["ip"])'; }
if check_then "antispoof: a rede cria-se" ok "$BIN" network create "$AS" \
   && check_then "antispoof: A1 (NET_ADMIN) arranca" ok \
        "$BIN" container run -d --name "$A1" --cap-add NET_ADMIN --net "$AS" "$IMG" sleep 900 \
   && check_then "antispoof: A2 arranca" ok "$BIN" container run -d --name "$A2" --net "$AS" "$IMG" sleep 900 \
   && check_then "antispoof: A3 arranca na namespace asteam" ok \
        "$BIN" container run -d --name "$A3" --namespace asteam --net "$AS" "$IMG" sleep 900 \
   && check_then "antispoof: A4 arranca na namespace asteam" ok \
        "$BIN" container run -d --name "$A4" --namespace asteam --net "$AS" "$IMG" sleep 900; then
  I2=$(ip_of "$A2"); I3=$(ip_of "$A3"); I4=$(ip_of "$A4")
  e0=$(echos_of "$A2"); "$BIN" container exec "$A1" ping -c3 -W1 "$I2" >/dev/null 2>&1
  check "antispoof: o tráfego legítimo chega ao vizinho" ok test "$(( $(echos_of "$A2") - e0 ))" -eq 3
  e0=$(echos_of "$A2")
  "$BIN" container exec "$A1" sh -c "ip addr add ${I2%.*}.250/16 dev eth0; ping -c3 -W1 -I ${I2%.*}.250 $I2" >/dev/null 2>&1
  check "antispoof: origem forjada não chega ao vizinho" ok test "$(( $(echos_of "$A2") - e0 ))" -eq 0
  e0=$(echos_of "$A3")
  "$BIN" container exec "$A1" sh -c "ip addr add $I4/16 dev eth0; ping -c3 -W1 -I $I4 $I3" >/dev/null 2>&1
  check "antispoof: forjar um membro de outra namespace não fura o isolamento" ok \
    test "$(( $(echos_of "$A3") - e0 ))" -eq 0
  "$BIN" container exec "$A1" sh -c "ip addr del ${I2%.*}.250/16 dev eth0; ip addr del $I4/16 dev eth0" >/dev/null 2>&1
  e0=$(echos_of "$A4"); "$BIN" container exec "$A3" ping -c3 -W1 "$I4" >/dev/null 2>&1
  check "antispoof: a mesma namespace continua aberta" ok test "$(( $(echos_of "$A4") - e0 ))" -eq 3
  "$BIN" container stop "$A2" >/dev/null 2>&1; "$BIN" container start "$A2" >/dev/null 2>&1
  # Wait for the CONDITION, not a sleep: a counter read before the restarted
  # container answers `exec` comes back empty and the arithmetic fails.
  for _ in $(seq 1 50); do [ -n "$(echos_of "$A2")" ] && break; sleep 0.2; done
  # The restarted container has a NEW veth and MAC; flush the sender's
  # neighbour cache or its pings go to the old MAC and prove nothing.
  I2=$(ip_of "$A2"); e0=$(echos_of "$A2")
  "$BIN" container exec "$A1" sh -c "ip neigh flush all; ping -c3 -W1 $I2" >/dev/null 2>&1
  check "antispoof: depois de um start o vizinho continua alcançável" ok test "$(( $(echos_of "$A2") - e0 ))" -eq 3
  e0=$(echos_of "$A2")
  # Same flush, on both sides: without it a stale neighbour entry loses the
  # packets with or without anti-spoofing, and the check passes on the defect.
  "$BIN" container exec "$A2" ip neigh flush all >/dev/null 2>&1
  "$BIN" container exec "$A1" sh -c "ip link set eth0 down; ip link set eth0 address 02:00:00:00:be:ef; ip link set eth0 up; ip neigh flush all; ping -c3 -W1 $I2" >/dev/null 2>&1
  check "antispoof: MAC forjado não chega ao vizinho" ok test "$(( $(echos_of "$A2") - e0 ))" -eq 0
fi
for c in "$A1" "$A2" "$A3" "$A4"; do e2e_rm "$c"; done
e2e_net_rm "$AS"

# The anti-spoofing GRANTS: a container that routes gets extra source prefixes,
# and an exceptional opt-out exists — both only when the node policy grants
# them, and every use is in the event log. No policy = nothing granted. Writes
# policy.json, so it runs on isolated state only.
if [[ "$E2E_ISOLATED" == 1 ]]; then
  GS="gs-$PFX"; GR="gr-$PFX"; GT="gt-$PFX"; GO="go-$PFX"
  POL="$DELONIX_ROOT/policy.json"; rm -f "$POL"
  check "antispoof: --allow-source sem política é recusado (77)" 77 \
    "$BIN" container run -d --name "$GR" --allow-source 10.99.1.0/24 --net none "$IMG" true
  check "antispoof: --no-source-check sem política é recusado (77)" 77 \
    "$BIN" container run -d --name "$GR" --no-source-check --net none "$IMG" true
  check "antispoof: a recusa diz DX-7801" ok bash -c \
    "'$BIN' container run -d --name $GR --allow-source 10.99.1.0/24 --net none '$IMG' true 2>&1 | grep -q DX-7801"
  printf '{"allowedSourcePrefixes":["10.99.0.0/16"],"allowSourceCheckOptOut":true}\n' > "$POL"
  check "antispoof: um prefixo fora da concessão é recusado" 77 \
    "$BIN" container run -d --name "$GR" --allow-source 10.98.0.0/24 --net none "$IMG" true
  if check_then "antispoof: concessões — a rede cria-se" ok "$BIN" network create "$GS" \
     && check_then "antispoof: concessões — o alvo arranca" ok \
          "$BIN" container run -d --name "$GT" --net "$GS" "$IMG" sleep 900 \
     && check_then "antispoof: concessões — o router com --allow-source concedido arranca" ok \
          "$BIN" container run -d --name "$GR" --cap-add NET_ADMIN --allow-source 10.99.1.0/24 --net "$GS" "$IMG" sleep 900 \
     && check_then "antispoof: concessões — o --no-source-check concedido arranca" ok \
          "$BIN" container run -d --name "$GO" --cap-add NET_ADMIN --no-source-check --net "$GS" "$IMG" sleep 900; then
    IT=$(ip_of "$GT")
    from_src() {  # <container> <source> → echo requests that reached GT
      local e0; e0=$(echos_of "$GT")
      "$BIN" container exec "$1" sh -c "ip addr add $2/32 dev eth0; ip neigh flush all; ping -c3 -W1 -I $2 $IT; ip addr del $2/32 dev eth0" >/dev/null 2>&1
      echo $(( $(echos_of "$GT") - e0 ))
    }
    check "antispoof: o router chega com uma origem dentro do prefixo concedido" ok test "$(from_src "$GR" 10.99.1.7)" -eq 3
    check "antispoof: o router é cortado fora do prefixo concedido" ok test "$(from_src "$GR" 10.99.2.7)" -eq 0
    check "antispoof: o opt-out concedido deixa passar qualquer origem" ok test "$(from_src "$GO" 10.77.0.9)" -eq 3
    check "antispoof: cada uso fica no registo de eventos" ok bash -c \
      "grep -q '\"source-prefixes-allowed\"' '$DELONIX_ROOT/events.jsonl' && grep -q '\"source-check-off\"' '$DELONIX_ROOT/events.jsonl'"
  fi
  for c in "$GR" "$GT" "$GO"; do e2e_rm "$c"; done
  e2e_net_rm "$GS"
  rm -f "$POL"
else
  skip "antispoof: concessões" "estado partilhado — o check escreveria o policy.json do nó real"
fi

# The private-egress guard (denyPrivateEgress / allowedEgressPrefixes): a workload
# on an engine network must not open connections into private address space
# through the uplink, unless the node policy grants the destination. The probe is
# a listener on the host's own primary address, which is what a workload reaches
# through the slirp, so it only runs when that address is private. Writes
# policy.json, so isolated state only; `policy unset` at the end lifts the guard
# from the running holder too.
if [[ "$E2E_ISOLATED" == 1 ]]; then
  PE="pe-$PFX"; PC="pc-$PFX"; POL="$DELONIX_ROOT/policy.json"; rm -f "$POL"
  HIP=$(ip -4 route get 1.1.1.1 2>/dev/null | sed -n 's/.* src \([0-9.]*\).*/\1/p')
  e2e_port HPORT
  is_private() { python3 -c 'import ipaddress,sys; a=ipaddress.ip_address(sys.argv[1]); sys.exit(0 if any(a in ipaddress.ip_network(n) for n in ("10.0.0.0/8","172.16.0.0/12","192.168.0.0/16","100.64.0.0/10")) else 1)' "$1" 2>/dev/null; }
  # The host half is the guard (a private primary address, a listener on it);
  # the engine half (the network, the container) is judged.
  if ! [[ -n "$HIP" ]] || ! is_private "$HIP" \
     || ! { python3 -m http.server --bind "$HIP" "$HPORT" >/dev/null 2>&1 & HSRV=$!; sleep 1; kill -0 "$HSRV" 2>/dev/null; }; then
    skip "private egress" "no private primary address on this host, or no listener could bind to it"
  elif check_then "private egress: the network is created" ok "$BIN" network create "$PE" \
     && check_then "private egress: its container starts" ok \
          "$BIN" container run -d --name "$PC" --net "$PE" "$IMG" sleep 900; then
    reach() { timeout 15 "$BIN" container exec "$PC" wget -q -T 3 -O /dev/null "http://$HIP:$HPORT/" >/dev/null 2>&1; }
    # Every `container run` hands the node policy to the running holder.
    sync_run() { "$BIN" container run --rm --name "ps-$PFX" --net "$PE" "$IMG" true >/dev/null 2>&1; }
    if reach; then
      printf '{"denyPrivateEgress":true,"denyHostNetwork":true}\n' > "$POL"; sync_run
      check "private egress: the guard is recorded for the holder" ok test -f "$DELONIX_ROOT/ingress/private-egress.json"
      check "private egress: the host's private address is refused once the policy denies it" fail reach
      printf '{"denyPrivateEgress":true,"denyHostNetwork":true,"allowedEgressPrefixes":["%s/32"]}\n' "$HIP" > "$POL"; sync_run
      check "private egress: a destination the node policy grants is reachable again" ok reach
      "$BIN" policy unset --force >/dev/null 2>&1
      check "private egress: policy unset lifts the guard from the running holder" ok reach
    else
      skip "private egress" "the host's private address is not reachable from a container even without a policy"
    fi
  fi
  [[ -n "${HSRV:-}" ]] && kill "$HSRV" 2>/dev/null
  e2e_rm "$PC"
  e2e_net_rm "$PE"
  rm -f "$POL"
else
  skip "private egress" "shared state — the check would write the real node's policy.json"
fi
# workloadSecurity: baseline (security audit #4): what `--privileged` is made of
# is refused by the node's ONE admission point, on `run` AND on the hot paths
# (`container update --volume-add`, `vm update --disk-add`), and a manifest VM
# goes through it like `vm create`. Every refusal is checked by the POLICY'S
# message, not the rc alone: a run that fails for another reason must not read
# as the policy working. Writes policy.json, so isolated state only.
if [[ "$E2E_ISOLATED" == 1 ]]; then
  WS="ws-$PFX"; POL="$DELONIX_ROOT/policy.json"; rm -f "$POL"
  refused() {  # <args…> → 0 when the run is refused BY THE POLICY, exit 77, and created nothing
    local out; out=$("$BIN" container run --rm --net none "$@" "$IMG" true 2>&1)
    [[ $? -eq 77 ]] && grep -q "workloadSecurity: baseline" <<<"$out"
  }
  printf '{"denyPrivileged":true}\n' > "$POL"
  check "baseline: denyPrivileged alone names the hole it leaves (lint)" ok bash -c \
    "'$BIN' container run --rm --net none '$IMG' true 2>&1 | grep -q POLICY-PRIVILEGE-EQUIVALENTS-OPEN"
  printf '{"workloadSecurity":"baseline"}\n' > "$POL"
  check "baseline: --cap-add ALL is refused" ok refused --cap-add ALL
  check "baseline: --cap-add SYS_ADMIN is refused" ok refused --cap-add SYS_ADMIN
  check "baseline: --host-pid is refused" ok refused --host-pid
  check "baseline: seccomp=unconfined is refused" ok refused --security-opt seccomp=unconfined
  check "baseline: a host-path bind mount is refused" ok refused -v /etc:/hostetc:ro
  check "baseline: a host device is refused" ok refused --device /dev/null
  check "baseline: --gpus dri is refused" ok refused --gpus dri
  check "baseline: a --masked-path list that drops the defaults is refused" ok refused --masked-path /x
  check "baseline: an ordinary container with an allowed capability and a named volume runs" ok \
    "$BIN" container run --rm --net none --cap-add NET_BIND_SERVICE -v "ws-vol-$PFX":/d "$IMG" true
  # The CRI's capability ceiling re-states the engine's whole default set as
  # `--cap-drop ALL` plus one `--cap-add` each (NET_RAW and NET_BROADCAST
  # included, which the PSS list does not have). That is what a plain run
  # gets, so it must run, with the same effective set; measured refused on
  # 2026-10-21 before `ContainerHardening::default_caps`, and no pod could
  # start on a baseline node. A capability beyond both lists still is not.
  DEFCAPS=(--cap-drop ALL)
  for c in CHOWN DAC_OVERRIDE FOWNER FSETID KILL SETGID SETUID SETPCAP NET_BIND_SERVICE \
           NET_BROADCAST NET_RAW SYS_CHROOT AUDIT_WRITE SETFCAP; do DEFCAPS+=(--cap-add "$c"); done
  check "baseline: the CRI's re-stated default capability set runs, with the plain run's CapEff" ok bash -c "
    plain=\$('$BIN' container run --rm --net none '$IMG' grep CapEff /proc/self/status 2>/dev/null)
    stated=\$('$BIN' container run --rm --net none ${DEFCAPS[*]} '$IMG' grep CapEff /proc/self/status 2>/dev/null)
    [[ -n \$plain && \$plain == \"\$stated\" ]]"
  check "baseline: …and SYS_ADMIN next to the default set is still refused" ok \
    refused "${DEFCAPS[@]}" --cap-add SYS_ADMIN
  if "$BIN" container run -d --name "$WS" --net none "$IMG" sleep 900 >/dev/null 2>&1; then
    check "baseline: update --volume-add of a host path is refused" ok bash -c \
      "'$BIN' container update '$WS' --volume-add /etc:/hostetc:ro 2>&1 | grep -q 'workloadSecurity: baseline'"
    check "baseline: …and the host path did not reach the container" fail \
      "$BIN" container exec "$WS" -- test -e /hostetc/hostname
    check "baseline: update --volume-add of a named volume still works" ok \
      "$BIN" container update "$WS" --volume-add "ws-vol-$PFX":/d
    # The CRI's argv shape (`exec <name> -- <cmd>`): a command that starts with
    # `-u 0` is the command, never exec's own --user.
    check "exec: a command after -- is never an option of exec (no uid 0)" fail bash -c \
      "'$BIN' container exec '$WS' -- -u 0 id 2>&1 | grep -q 'uid=0'"
  else
    skip "baseline: update --volume-add" "could not start the container"
  fi
  "$BIN" container rm -f "$WS" >/dev/null 2>&1
  printf '{"denyDevicePassthrough":true}\n' > "$POL"
  cat > "$OUT/ws-vm-$PFX.yaml" <<YAML
apiVersion: compute.delonix.io/v1alpha1
kind: VirtualMachine
metadata: { name: ws-vm-$PFX }
spec:
  disk: /nonexistent/golden.qcow2
  devices: ["0000:01:00.0"]
YAML
  check "vm admission: a kind: VirtualMachine with devices is refused like vm create --device" ok bash -c \
    "'$BIN' stack apply -f '$OUT/ws-vm-$PFX.yaml' 2>&1 | grep -q 'device passthrough (0000:01:00.0'"
  ln -sf /dev/null "$OUT/ws-disk-$PFX.qcow2"
  check "vm admission: vm update --disk-add of a symlink to a device is refused" ok bash -c \
    "'$BIN' vm update ws-novm --disk-add '$OUT/ws-disk-$PFX.qcow2' 2>&1 | grep -q 'device passthrough'"
  rm -f "$POL" "$OUT/ws-disk-$PFX.qcow2"
  # A RuntimePolicy field converges in place: before the fix, turning
  # denyPrivateEgress on in an applied manifest planned an impossible `-/+`.
  rp() { printf 'apiVersion: security.delonix.io/v1alpha1\nkind: RuntimePolicy\nmetadata: { name: node }\nspec:\n  denyPrivateEgress: %s\n' "$1" > "$OUT/ws-rp-$PFX.yaml"; }
  rp false; "$BIN" stack apply -f "$OUT/ws-rp-$PFX.yaml" >/dev/null 2>&1
  rp true
  check "RuntimePolicy: denyPrivateEgress false→true plans an in-place update, not a replace" ok bash -c \
    "out=\$('$BIN' stack plan -f '$OUT/ws-rp-$PFX.yaml' 2>&1); grep -q '^  ~ *RuntimePolicy/node' <<<\"\$out\" && ! grep -q '[-]/+' <<<\"\$out\""
  "$BIN" stack apply -f "$OUT/ws-rp-$PFX.yaml" >/dev/null 2>&1
  check "RuntimePolicy: after the apply the plan has nothing to do" 0 \
    "$BIN" stack plan -f "$OUT/ws-rp-$PFX.yaml" --detailed-exitcode
  "$BIN" policy unset --force >/dev/null 2>&1
  "$BIN" volume rm "ws-vol-$PFX" >/dev/null 2>&1
  rm -f "$POL"
else
  skip "workloadSecurity baseline" "shared state — the check would write the real node's policy.json"
fi
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
e2e_rm "ed-$PFX"
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
if check_then "drift: stack apply do container de teste" ok "$BIN" stack apply -f "$DRW/delonix-manifest.yaml"; then
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
fi
e2e_rm "dr-$PFX"

check "stack destroy --dry-run" ok "$BIN" stack destroy -f "$WORK/delonix-manifest.yaml" --dry-run
check "stack destroy" ok "$BIN" stack destroy -f "$WORK/delonix-manifest.yaml"
# O destroy levou o que a stack possui — o `describe` a seguir tem de correr na
# mesma (parte do ficheiro, não de um registo), mas os recursos já não existem.
check "volume describe depois do destroy recusa" 4 "$BIN" volume describe "sv-$PFX"
"$BIN" volume rm "sv-$PFX" >/dev/null 2>&1
e2e_net_rm "sn-$PFX"

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
  # Uma recusa da política é de PERMISSÃO (DX-7802), exit 77 — nunca o 1 de um
  # pedido mal formado.
  check "com política, o run é recusado (exit 77)" 77 \
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
  e2e_rm "$ADOPT"
  rm -f "$OUT/adopt.yaml"
fi

# 2026-10-02: o `env` de um container a correr mudou no manifesto, o plano disse
# `=` com «FieldsNotCompared … Recreate it (`--replace Container/<name>`)», e esse
# `--replace` não recriava nada — só autorizava um `Replace` que o plano já
# propusesse, e um campo não comparado nunca o propõe. O `inspect` ficava com o
# env antigo e o único caminho era `container rm -f` + `apply`. Nomear o recurso
# é agora o próprio diff; a prova é o `inspect`, não o rc do apply.
if [[ $E2E_HAVE_IMAGE -eq 1 ]]; then
  RPW="$WORK/replace-named"; mkdir -p "$RPW"; RPC="rp-$PFX"
  cat >"$RPW/delonix-manifest.yaml" <<YAML
apiVersion: compute.delonix.io/v1alpha1
kind: Container
metadata:
  name: $RPC
spec:
  image: $IMG
  command: ["sleep", "600"]
  network: host
  restart: "no"
  env: ["RP_MARK=old"]
YAML
  if check_then "stack apply --replace nomeado: o apply do container de teste" ok \
       "$BIN" stack apply -f "$RPW/delonix-manifest.yaml"; then
    sed -i 's/RP_MARK=old/RP_MARK=new/' "$RPW/delonix-manifest.yaml"
    # Pelo `-o json`: o código da condição só sai no JSON (o plano humano diz a
    # frase, e traduzida), e o JSON não muda com a locale (ADR-0005).
    check "stack plan: um env mudado não é comparado, e o plano DIZ-O" ok bash -c "
      '$BIN' stack plan -f '$RPW/delonix-manifest.yaml' -o json 2>/dev/null | grep -q 'FieldsNotCompared'"
    check "stack apply --replace Container/<nome> recria mesmo sem diff no plano" ok bash -c "
      out=\$('$BIN' stack apply -f '$RPW/delonix-manifest.yaml' --replace 'Container/$RPC' 2>&1) || { printf '%s\n' \"\$out\"; exit 1; }
      printf '%s\n' \"\$out\"
      printf '%s' \"\$out\" | grep -q recreating || { echo 'o apply não recriou'; exit 1; }"
    check "... e o container recriado tem o env NOVO (container inspect)" ok bash -c "
      out=\$('$BIN' container inspect '$RPC' 2>&1)
      printf '%s' \"\$out\" | grep -q 'RP_MARK=new' || { echo 'env novo ausente'; exit 1; }
      printf '%s' \"\$out\" | grep -q 'RP_MARK=old' && { echo 'o env antigo ficou'; exit 1; }
      exit 0"
    check "... e a seguir o plano não tem nada a propor" 0 \
      "$BIN" stack plan -f "$RPW/delonix-manifest.yaml" --detailed-exitcode
    # A condição dizia «declared but NOT applied: command, env, restart» logo
    # depois da recriação que os tinha aplicado — e o `restart` é o alias do
    # `restartPolicy`, que É comparado. Com o registo da criação
    # (`delonix.io/created-spec`) o plano compara o manifesto com o que o
    # container recebeu ao nascer: nada mudou, nada a dizer.
    check "... e o plano já não diz «não aplicado» do que a recriação aplicou" ok bash -c "
      out=\$('$BIN' stack plan -f '$RPW/delonix-manifest.yaml' -o json 2>/dev/null) || exit 1
      printf '%s' \"\$out\" | grep -q FieldsNotCompared && { printf '%s\n' \"\$out\"; exit 1; }
      exit 0"
    sed -i 's/RP_MARK=new/RP_MARK=newer/' "$RPW/delonix-manifest.yaml"
    # E quando muda outra vez, nomeia SÓ o que mudou (o `command` aparecia ao
    # lado do `env` sem ter mexido) e o conselho traz o nome deste container.
    check "uma nova mudança de env nomeia só o env, e o --replace com o nome" ok bash -c "
      '$BIN' stack plan -f '$RPW/delonix-manifest.yaml' -o json 2>/dev/null | python3 -c '
import json, sys
msgs = [c[\"message\"] for ch in json.load(sys.stdin) for c in ch.get(\"conditions\", [])
        if c.get(\"reason\") == \"FieldsNotCompared\"]
print(msgs)
assert len(msgs) == 1 and \": env — \" in msgs[0] and \"Container/$RPC\" in msgs[0]
'"
    check "--replace com o Kind em minúsculas também é aceite (não é um typo)" ok \
      "$BIN" stack apply -f "$RPW/delonix-manifest.yaml" --replace "container/$RPC"
    # Um rollback É um apply (ADR-0019) e passa pelo mesmo `--replace`. Mas
    # re-aplica o manifesto RENDERIZADO (todos os defaults escritos), e contra
    # um registo do manifesto cru o aviso nomeava 31 campos que ninguém tinha
    # mexido — medido a 2026-10-02. Os dois lados comparam-se com defaults.
    check "stack rollback --replace repõe o env da revisão 1, e o aviso nomeia só o env" ok bash -c "
      out=\$('$BIN' stack rollback -f '$RPW/delonix-manifest.yaml' --to 1 --replace 'Container/$RPC' 2>&1) || { printf '%s\n' \"\$out\"; exit 1; }
      printf '%s\n' \"\$out\"
      printf '%s' \"\$out\" | grep FieldsNotCompared | grep -qv ': env — ' && { echo 'o aviso nomeou mais do que o env'; exit 1; }
      '$BIN' container inspect '$RPC' | grep -q 'RP_MARK=old' || { echo 'o rollback não repôs o env'; exit 1; }"
    "$BIN" stack destroy -f "$RPW/delonix-manifest.yaml" >/dev/null 2>&1
  fi
  e2e_rm "$RPC"
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

# Uma gralha DENTRO de um bloco do Volume (ADR-0069 D1). Medido na origin/main
# a 2026-10-22: `spec.nfs.readOnlyy: true` passava o `validate` com «OK», o
# `plan` com rc=2 sem uma palavra, e o `apply` só avisava e seguia para o
# efeito — a montar o export em ESCRITA. `spec.share.fromm` nem aviso tinha. A
# gralha no topo (`spec.quotaa`) já era recusada pelo load; esta passava porque
# a verificação dos blocos só corria no apply, depois do load.
NFW="$OUT/nested-typo-$PFX"; mkdir -p "$NFW"
for blk in \
  "nfs|nfs: { server: 192.0.2.1, share: /export/x, readOnlyy: true }|spec.nfs.readOnlyy" \
  "share|share: { from: nfpai-$PFX, fromm: y }|spec.share.fromm" \
  "truenas|provision: { truenas: { url: 'https://192.0.2.1', dataset: tank/x, datasett: tank/y } }|spec.provision.truenas.datasett"
do
  IFS='|' read -r tag spec field <<<"$blk"
  printf 'apiVersion: storage.delonix.io/v1alpha1\nkind: Volume\nmetadata: { name: nf%s-%s }\nspec: { %s }\n' \
    "$tag" "$PFX" "$spec" >"$NFW/$tag.yaml"
  check "manifesto: gralha em $field — o validate nomeia-a e não diz OK; --strict sai 1" ok bash -c "
    out=\$('$BIN' manifest validate -f '$NFW/$tag.yaml' 2>&1); printf '%s\n' \"\$out\"
    grep -q \"'${field##*.}'\" <<<\"\$out\" || { echo 'o validate não nomeou o campo'; exit 1; }
    grep -q 'OK —' <<<\"\$out\" && { echo 'o validate disse OK'; exit 1; }
    '$BIN' manifest validate --strict -f '$NFW/$tag.yaml' >/dev/null 2>&1; [[ \$? == 1 ]]"
  check "manifesto: gralha em $field — plan e apply recusam com 1, nada aplicado, nada criado" ok bash -c "
    for verb in 'stack plan --detailed-exitcode' 'stack apply'; do
      out=\$(timeout 60 '$BIN' \$verb -f '$NFW/$tag.yaml' 2>&1); rc=\$?; printf '%s: rc=%s\n%s\n' \"\$verb\" \$rc \"\$out\"
      [[ \$rc == 1 ]] || { echo \"\$verb: rc=\$rc, esperado 1\"; exit 1; }
      grep -q 'nothing was applied' <<<\"\$out\" && grep -q 'Volume .nf$tag-$PFX. $field' <<<\"\$out\" \
        || { echo \"\$verb: a recusa não nomeia $field\"; exit 1; }
    done
    '$BIN' volume ls 2>/dev/null | grep -qw 'nf$tag-$PFX' && { echo 'o apply criou o volume'; exit 1; }
    exit 0"
  "$BIN" volume rm -f "nf$tag-$PFX" >/dev/null 2>&1
done
rm -rf "$NFW"

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
    e2e_rm "$_n"
    if check_then "-m $_v: o container arranca" ok \
         "$BIN" container run -d --name "$_n" --net none -m "$_v" "$IMG" sleep 60; then
      # A prova é o CGROUP, não o registo: era exactamente aí que os dois
      # discordavam, com o registo a dizer 64Mi e o kernel a dizer `max`.
      check "-m $_v chega ao kernel como $_want" ok \
        bash -c "[ \"\$(_cg_of $_n memory.max)\" = $_want ]" || true
    fi
    e2e_rm "$_n"
  done
  # O MESMO `-m` numa rede própria. Todos os checks de limites desta bateria
  # corriam com `--net none`, e foi por aí que isto passou: a 2.ª passagem do
  # `--net <rede>` corre sob `ip netns exec`, cujo sysfs novo tapa o cgroup2, e
  # a sonda do preflight lia um `/sys/fs/cgroup` vazio como «sem delegação» —
  # o container era RECUSADO (exit 69) numa sessão com delegação, enquanto o
  # `spawn` (que destapa antes de olhar) aplicava o limite sem problema.
  # Medido 2026-09-30 com o template `odoo`, o único com `network:`.
  _ln="${PFX}limnet"; _lc="${PFX}limnetc"
  e2e_rm "$_lc"
  if check_then "-m 64M numa rede própria: a rede cria-se" ok "$BIN" network create "$_ln"; then
    check "-m 64M numa rede própria arranca (não é recusado)" ok \
      "$BIN" container run -d --name "$_lc" --net "$_ln" -m 64M "$IMG" sleep 60
    check "-m 64M numa rede própria chega ao kernel como 67108864" ok \
      bash -c "[ \"\$(_cg_of $_lc memory.max)\" = 67108864 ]" || true
    e2e_rm "$_lc"
    e2e_net_rm "$_ln"
  fi
  # A outra metade: o que não se consegue ler é RECUSADO antes de criar seja o
  # que for. `64MB` e `99999999999T` estão aqui de propósito — o primeiro é a
  # grafia que toda a gente tenta, o segundo saturava para u64::MAX, que escrito
  # em `memory.max` é indistinguível de não haver limite.
  for bad in abc 64MB "99999999999T"; do
    check "-m $bad é recusado, não ignorado" fail \
      "$BIN" container run -d --name "${PFX}limbad" --net none -m "$bad" "$IMG" true
    e2e_rm "${PFX}limbad"
  done
  # `500m` é millicores do Kubernetes. Recusar é o ponto: convertê-lo seria o
  # motor a adivinhar, e adivinhar dava 1 CPU inteiro.
  for bad in abc 500m; do
    check "--cpus $bad é recusado, não ignorado" fail \
      "$BIN" container run -d --name "${PFX}limcpu" --net none --cpus "$bad" "$IMG" true
    e2e_rm "${PFX}limcpu"
  done
  # `--restart` é o de aresta mais afiada da classe: uma gralha (`alwyas`,
  # `on-failre`) era aceite com rc=0 e deixava `restart_policy: null` — um
  # serviço que nunca volta a subir, em silêncio, com o operador convencido de
  # que configurou um supervisor. Os outros desperdiçam recursos; este perde
  # disponibilidade.
  for bad in talvez alwyas on-failure:0 always:2; do
    check "--restart $bad é recusado, não ignorado" fail \
      "$BIN" container run -d --name "${PFX}limrs" --net none --restart "$bad" "$IMG" true
    e2e_rm "${PFX}limrs"
  done
  # Os pesos documentavam `1–10000` e não verificavam nada: `0`, `99999` e `abc`
  # caíam todos no default do kernel (100), com rc=0. Uma gama no `--help` que o
  # código não impõe é uma promessa, não um facto.
  for bad in 0 10001 abc; do
    check "--cpu-weight $bad é recusado, não ignorado" fail \
      "$BIN" container run -d --name "${PFX}limw" --net none --cpu-weight "$bad" "$IMG" true
    e2e_rm "${PFX}limw"
  done
  # E o que passa tem de CHEGAR ao registo: aceitar a política e não a guardar é
  # o mesmo defeito com outra roupa.
  e2e_rm "${PFX}limrs"
  if check_then "--restart on-failure:3: o container arranca" ok \
       "$BIN" container run -d --name "${PFX}limrs" --net none --restart on-failure:3 "$IMG" sleep 30; then
    check "--restart on-failure:3 fica no registo" ok \
      bash -c "'$BIN' container inspect ${PFX}limrs | grep -q 'on-failure:3'" || true
  fi
  e2e_rm "${PFX}limrs"
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
  e2e_rm "$_n"
  if check_then "sem flags: o container arranca" ok "$BIN" container run -d --name "$_n" --net none "$IMG" sleep 60; then
    check "sem flags: a leaf TEM tecto de memória (não 'max')" ok \
      bash -c '[ "$(cat "$(_leaf_of '"$_n"')/memory.max")" != max ]'
    # Sem isto o tecto de memória é contornável: as páginas saem para swap e o
    # container passa do que lhe foi permitido sem nunca tocar em `memory.max`.
    check "sem flags: memory.swap.max=0 (não se escapa ao tecto pelo swap)" ok \
      bash -c '[ "$(cat "$(_leaf_of '"$_n"')/memory.swap.max")" = 0 ]'
    check "sem flags: pids.max=512 (anti fork-bomb por omissão)" ok \
      bash -c '[ "$(cat "$(_leaf_of '"$_n"')/pids.max")" = 512 ]'
  fi
  e2e_rm "$_n"

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
  e2e_rm "$_n"
  if ! check_then "--cpus 0.5: o container arranca" ok "$BIN" container run -d --name "$_n" --net none --cpus 0.5 "$IMG" \
       sh -c 'i=0; while [ $i -lt 4 ]; do (while :; do :; done) & i=$((i+1)); done; sleep 120'; then
    :  # já é um FAIL, com o erro do run
  elif sleep 3 && [ -r "$(_leaf_of "$_n" 2>/dev/null)/cpu.stat" ]; then
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
  e2e_rm "$_n"

  # --- O TECTO AGREGADO: a soma de todos também tem limite ------------------
  #
  # Cada container pode estar dentro do seu tecto e a SOMA matar o host na mesma.
  # É a razão de existir do orçamento no cgroup pai (85% do host por omissão);
  # em rootless ele não existiu durante versões, e «sem limite» era mesmo sem
  # limite. Verifica-se onde os workloads deste utilizador realmente vivem: o
  # PAI da leaf, seja ele a `delonix.slice` (root) ou a base delegada (rootless).
  _n="${PFX}enfagg"
  e2e_rm "$_n"
  if check_then "tecto agregado: o container arranca" ok "$BIN" container run -d --name "$_n" --net none "$IMG" sleep 60; then
    check "o cgroup PAI tem tecto agregado de memória (a soma não mata o host)" ok \
      bash -c '[ "$(cat "$(dirname "$(_leaf_of '"$_n"')")/memory.max")" != max ]'
    check "…e tecto agregado de pids" ok \
      bash -c '[ "$(cat "$(dirname "$(_leaf_of '"$_n"')")/pids.max")" != max ]'
  fi
  e2e_rm "$_n"

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

  # --- FUGA: um arranque recusado DEPOIS do clone não deixa o cgroup -------
  #
  # Tudo o que falha entre o `clone` e o registo (os mapas de userns, o
  # `setup_cgroup`, o hook `on_started` que liga o slirp) parava no SIGKILL: o
  # caminho com userns devolvia sem remover o cgroup que o `setup_cgroup` acabara
  # de criar, e o caminho sem userns removia-o com o processo ainda lá dentro. O
  # registo nunca chega a ser escrito, por isso ninguém voltaria a encontrar a
  # leaf. Medido 2026-09-28: `run -d -p` com um `slirp4netns` que falha deixou um
  # `dlx-<id>` VAZIO no host em 4 corridas de 4. Um `slirp4netns` falso no PATH
  # é o disparo: o hook corre depois de o cgroup existir. O id vem do evento
  # `create`, o único rasto que um arranque recusado deixa.
  check "um arranque recusado depois do clone não deixa o cgroup do container para trás" ok bash -c '
    fb="$(dirname "$DELONIX_ROOT")/fakeslirp"; mkdir -p "$fb"
    printf "#!/bin/sh\nexit 1\n" > "$fb/slirp4netns"; chmod +x "$fb/slirp4netns"
    port=$(python3 -c "import socket;s=socket.socket();s.bind((\"127.0.0.1\",0));print(s.getsockname()[1])")
    ev0=$(cat "$DELONIX_ROOT/events.jsonl" 2>/dev/null | wc -l)
    if PATH="$fb:$PATH" timeout 60 "$BIN" container run -d -p "$port:80" "$IMG" sleep 60 >/dev/null 2>&1; then
      rm -rf "$fb"
      echo "o run arrancou com um slirp4netns que falha — não é o caso que se quer medir"; exit 1
    fi
    rm -rf "$fb"
    id=$(tail -n +$((ev0+1)) "$DELONIX_ROOT/events.jsonl" 2>/dev/null | python3 -c "
import json,sys
ids=[e[\"id\"] for e in map(json.loads,sys.stdin) if e.get(\"action\")==\"create\"]
print(ids[-1] if ids else \"\")")
    [ -n "$id" ] || { echo "sem evento create: o run falhou antes do clone, e não há o que medir"; exit 1; }
    left=$(find /sys/fs/cgroup -maxdepth 7 -type d \( -name "dlx-$id" -o -name "delonix-$id" \) 2>/dev/null)
    [ -z "$left" ] || { echo "ficou para trás: $left (procs: $(cat $left/cgroup.procs 2>/dev/null | tr "\n" " "))"; exit 1; }
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

# ------------------------------------------------------------------------------
# `metadata.labels` of a Network and of a Volume reach the record and converge.
# Until 2026-10-03 only a Container received them: a network or a volume applied
# from a manifest carried the ownership stamp and nothing the document declared.
# The CYCLE is what proves it — apply, read the record, plan unchanged, change a
# label, apply, read again.
# ------------------------------------------------------------------------------
NLDIR="$OUT/nl-labels"; mkdir -p "$NLDIR"
nl_manifest() {
  printf '%s\n' 'apiVersion: networking.delonix.io/v1alpha1' 'kind: Network' \
    "metadata: { name: nl-net, labels: $1 }" 'spec: {}' '---' \
    'apiVersion: storage.delonix.io/v1alpha1' 'kind: Volume' \
    "metadata: { name: nl-vol, labels: $1 }" 'spec: {}' > "$NLDIR/m.yaml"
}
nl_labels() { # the labels the engine has for both, as "<network>|<volume>"
  local n v
  n="$("$BIN" network describe nl-net 2>/dev/null | grep -o 'app=[a-z]*\|tier=[a-z]*' | sort | tr '\n' ',')"
  v="$("$BIN" volume describe nl-vol 2>/dev/null | grep -o 'app=[a-z]*\|tier=[a-z]*' | sort | tr '\n' ',')"
  echo "$n|$v"
}
nl_manifest '{ app: web, tier: front }'
check "stack apply: metadata.labels de um Network e de um Volume chegam ao registo" ok bash -c \
  "cd '$NLDIR' && '$BIN' stack apply -f m.yaml >/dev/null 2>&1 && [[ \"\$($(declare -f nl_labels); BIN='$BIN' nl_labels)\" == 'app=web,tier=front,|app=web,tier=front,' ]]"
check "um manifesto inalterado não mostra deriva nos labels (plan --detailed-exitcode = 0)" 0 \
  bash -c "cd '$NLDIR' && '$BIN' stack plan -f m.yaml --detailed-exitcode >/dev/null 2>&1"
nl_manifest '{ app: api }'
check "mudar e tirar um label é uma alteração do plano (2), converge a quente e sai do registo" ok bash -c \
  "cd '$NLDIR' && '$BIN' stack plan -f m.yaml --detailed-exitcode >/dev/null 2>&1; [[ \$? == 2 ]] && '$BIN' stack apply -f m.yaml >/dev/null 2>&1 && [[ \"\$($(declare -f nl_labels); BIN='$BIN' nl_labels)\" == 'app=api,|app=api,' ]] && '$BIN' stack plan -f m.yaml --detailed-exitcode >/dev/null 2>&1"
(cd "$NLDIR" && "$BIN" stack destroy -f m.yaml >/dev/null 2>&1)
printf '%s\n' 'apiVersion: networking.delonix.io/v1alpha1' 'kind: Network' \
  'metadata: { name: nl-pk-net, labels: { app: web } }' 'spec: {}' '---' \
  'apiVersion: storage.delonix.io/v1alpha1' 'kind: Volume' \
  'metadata: { name: nl-pk-vol, labels: { app: web } }' 'spec: {}' > "$NLDIR/pk.yaml"
check "network apply / volume apply fora de uma stack também escrevem os labels do documento" ok bash -c \
  "'$BIN' network apply -f '$NLDIR/pk.yaml' >/dev/null 2>&1 && '$BIN' volume apply -f '$NLDIR/pk.yaml' >/dev/null 2>&1 && '$BIN' network describe nl-pk-net | grep -q 'app=web' && '$BIN' volume describe nl-pk-vol | grep -q 'app=web'"
e2e_net_rm nl-pk-net; "$BIN" volume rm nl-pk-vol >/dev/null 2>&1
# The same for a `kind: VirtualMachine` — measured dropping them too. Labels are
# the one VM field that converges in place: the domain keeps its id.
if libvirt_system_usable; then
  NLVM="nl-vm-$PFX"; qemu-img create -f qcow2 "$NLDIR/vm.qcow2" 64M >/dev/null 2>&1
  nl_vm_manifest() {
    printf '%s\n' 'apiVersion: compute.delonix.io/v1alpha1' 'kind: VirtualMachine' \
      "metadata: { name: $NLVM, labels: $1 }" \
      "spec: { disk: $NLDIR/vm.qcow2, backend: libvirt, memory: 256M }" > "$NLDIR/vm.yaml"
  }
  nl_vm_labels() { "$BIN" describe vm "$NLVM" 2>/dev/null | grep -o 'app=[a-z]*\|tier=[a-z]*' | sort | tr '\n' ','; }
  nl_vm_manifest '{ app: web, tier: front }'
  if check_then "stack apply de uma VirtualMachine libvirt (labels)" ok \
       bash -c "cd '$NLDIR' && '$BIN' stack apply -f vm.yaml"; then
    NLDOM="$(virsh -c qemu:///system domid "$NLVM" 2>/dev/null | head -1)"
    check "stack apply: metadata.labels de uma VirtualMachine chegam ao registo, sem deriva no plano" ok bash -c \
      "[[ '$(nl_vm_labels)' == 'app=web,tier=front,' ]] && cd '$NLDIR' && '$BIN' stack plan -f vm.yaml --detailed-exitcode >/dev/null 2>&1"
    nl_vm_manifest '{ app: api }'
    (cd "$NLDIR" && "$BIN" stack apply -f vm.yaml >/dev/null 2>&1)
    check "mudar os labels de uma VM converge a quente: o registo segue e o domínio é o mesmo" ok bash -c \
      "[[ '$(nl_vm_labels)' == 'app=api,' ]] && [[ \"\$(virsh -c qemu:///system domid '$NLVM' | head -1)\" == '$NLDOM' ]] && cd '$NLDIR' && '$BIN' stack plan -f vm.yaml --detailed-exitcode >/dev/null 2>&1"
  fi
  (cd "$NLDIR" && "$BIN" stack destroy -f vm.yaml >/dev/null 2>&1)
else
  skip "labels de uma VirtualMachine" "precisa de virsh, qemu-img, qemu-system-x86_64 e de uma ligação qemu:///system utilizável"
fi

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
# O contrário: um `--replace` sem nada a mudar destrói e recria o volume, e isso
# é pedir alguma coisa à máquina — grava uma revisão, com `replace` no resumo.
# A primeira versão da correcção do `kept` perdia-a (o `NoOp` promovido ficava
# com `changed: false`), e só a sonda do cli-tester o viu.
check "um --replace sem alterações grava a revisão da recriação" ok bash -c "
  n() { '$BIN' stack history -f '$HWORK/hist.yaml' -o json | python3 -c 'import json,sys; print(len(json.load(sys.stdin)))'; }
  before=\$(n); '$BIN' stack apply -f '$HWORK/hist.yaml' --replace 'Volume/hv-$PFX' >/dev/null 2>&1 || exit 1
  '$BIN' stack history -f '$HWORK/hist.yaml' -o json | python3 -c 'import json,sys
h=json.load(sys.stdin); s=h[-1].get(\"summary\") or {}
print(len(h), s); sys.exit(0 if len(h)==int(sys.argv[1])+1 and s.get(\"replace\")==1 else 1)' \"\$before\""
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
check "…e já não está" 4 "$BIN" volume inspect "ov-$PFX"

# `stack apply` de um recurso que OUTRA stack possui: classe de conflito (rc 5,
# DX-5601), e nada mudado. Até ao #902 saía rc 1/DX-1000 — «argumento
# inválido», o mesmo de um `--replace` em falta —, e quem lê o rc ia atrás da
# flag errada: nenhuma flag autoriza um conflito. Os três testes unitários do
# `refusal()` provam a função; só este prova que o binário a usa no caminho
# real do apply, contra registos reais.
#
# O rc sozinho não chega: uma recusa com 5 DEPOIS de mexer no recurso passaria.
# Por isso cada recusa exige também o código DX na saída, e a seguir mede-se o
# registo de cada recurso (é nele que vive o carimbo `delonix.io/stack`) contra
# o que estava antes, e que a stack recusada não ganhou revisão — contada antes
# e depois, nunca contra um 0 fixo: o `stack destroy` não apaga a história. O
# rc gravado no JSONL é o do `apply`; 99 quer dizer rc certo e código DX errado.
CFWORK="$WORK/conflito-$PFX"; mkdir -p "$CFWORK"
CSA="stk-a-$PFX"; CSB="stk-b-$PFX"; CFV="cfv-$PFX"; CFN="cfn-$PFX"; CFB="cfb-$PFX"
# O root como o motor o resolve (`delonix-state`, `Store::root`): sem
# DELONIX_ROOT, `$XDG_DATA_HOME/delonix` antes de `~/.local/share/delonix`.
CROOT="${DELONIX_ROOT:-${XDG_DATA_HOME:-$HOME/.local/share}/delonix}"
cat >"$CFWORK/a.yaml" <<YAML
apiVersion: delonix.io/v1
kind: Volume
metadata: { name: $CFV }
spec: {}
---
apiVersion: networking.delonix.io/v1alpha1
kind: Network
metadata: { name: $CFN }
spec: {}
YAML
# Um volume que é MESMO da stk-b, e o mesmo com `mountOptions` declarado: não
# converge a quente, logo é um Replace que nenhum `--replace` autorizou. Um
# volume e não uma rede com a `subnet` mudada, e a primeira versão era a rede:
# a subnet automática da `$CFN` deriva do nome, e numa corrida calhou em
# 10.238.0.0/16 — por cima da subnet fixa, e o apply da stk-b saiu DX-5303.
printf 'apiVersion: delonix.io/v1\nkind: Volume\nmetadata: { name: %s }\nspec: {}\n' "$CFB" >"$CFWORK/b.yaml"
printf 'apiVersion: delonix.io/v1\nkind: Volume\nmetadata: { name: %s }\nspec: { mountOptions: noatime }\n' "$CFB" \
  >"$CFWORK/b2.yaml"
{ cat "$CFWORK/b2.yaml"; printf -- '---\napiVersion: delonix.io/v1\nkind: Volume\nmetadata: { name: %s }\nspec: {}\n' "$CFV"; } \
  >"$CFWORK/misto.yaml"
# Sem `2>&1`, e de propósito: um registo em falta (layout mudado, CROOT errado)
# dava a MESMA mensagem de erro antes e depois, e a comparação passava sem ter
# medido nada. Assim o baseline fica vazio e o check chumba pelo rc.
cf_sums() { (cd "$CROOT" && sha256sum "volumes/$CFV/meta.json" "networks/$CFN" "$@"); }
cf_revs() { "$BIN" stack history --name "$1" -o json | python3 -c 'import json,sys; print(len(json.load(sys.stdin)))'; }
if check_then "conflito de posse: a stk-a aplica um volume e uma rede" ok \
     "$BIN" stack apply -f "$CFWORK/a.yaml" --name "$CSA"; then
  CF_A=$(cf_sums); CF_AREVS=$(cf_revs "$CSB")
  check "conflito de posse: o mesmo manifesto sob outra stack sai 5 com DX-5601" 5 bash -c "
    out=\$('$BIN' stack apply -f '$CFWORK/a.yaml' --name '$CSB' 2>&1); rc=\$?
    printf '%s\n' \"\$out\"
    [[ \$rc == 5 ]] && ! grep -q DX-5601 <<<\"\$out\" && exit 99
    exit \$rc"
  # O `plan --detailed-exitcode` sobre o MESMO conflito. Até ao #911 saía 0
  # (o `Conflict` vinha com `changed: false` e o plano só olhava para isso): um
  # portão de deriva em CI ficava verde e o `apply` a seguir recusava com 5. O
  # #911 só tinha testes unitários; isto prova o binário no caminho real, contra
  # registos com o carimbo de outra stack. Corre ANTES do «nada mudou» abaixo,
  # que assim prova também que o plano não escreveu nada.
  check "conflito de posse: stack plan --detailed-exitcode sai 5 com DX-5601, como o apply" 5 bash -c "
    out=\$('$BIN' stack plan -f '$CFWORK/a.yaml' --name '$CSB' --detailed-exitcode 2>&1); rc=\$?
    printf '%s\n' \"\$out\"
    [[ \$rc == 5 ]] && ! grep -q DX-5601 <<<\"\$out\" && exit 99
    exit \$rc"
  # Com `-o json` o stdout continua a ser o plano, JSON válido e com os dois
  # conflitos a nomear o dono; o DX vai para o stderr. Um 5 com o stdout partido
  # rebentava o `jq` do portão que o lê.
  check "…com -o json: sai 5, o stdout é o plano em JSON e o DX-5601 vai para o stderr" 5 bash -c "
    js=\$('$BIN' stack plan -f '$CFWORK/a.yaml' --name '$CSB' -o json --detailed-exitcode 2>'$CFWORK/plan.err'); rc=\$?
    printf '%s\n' \"\$js\"; cat '$CFWORK/plan.err'
    [[ \$rc == 5 ]] || exit \$rc
    grep -q DX-5601 '$CFWORK/plan.err' || { echo 'sem DX-5601 no stderr'; exit 99; }
    python3 -c 'import json,sys
p=json.loads(sys.argv[1])
got=sorted((r[\"kind\"],r[\"name\"]) for r in p if r[\"action\"]==\"conflict\" and r.get(\"owner\")==sys.argv[2])
sys.exit(0 if got==[(\"Network\",sys.argv[4]),(\"Volume\",sys.argv[3])] else 1)' \"\$js\" '$CSA' '$CFV' '$CFN' \
      || { echo 'o stdout não é o plano com os dois conflitos da $CSA'; exit 99; }
    exit 5"
  check "…o plan de topo responde o mesmo 5" 5 bash -c "
    out=\$('$BIN' plan -f '$CFWORK/a.yaml' --name '$CSB' --detailed-exitcode 2>&1); rc=\$?
    printf '%s\n' \"\$out\"
    [[ \$rc == 5 ]] && ! grep -q DX-5601 <<<\"\$out\" && exit 99
    exit \$rc"
  # Sem a flag, o contrato do `plan` não muda: mostra o conflito e sai 0.
  check "…e sem --detailed-exitcode o plano do conflito continua a sair 0" ok \
    "$BIN" stack plan -f "$CFWORK/a.yaml" --name "$CSB"
  # O `apply --dry-run` da stk-b responde o que o apply respondeu: até aqui
  # saía 0 sem olhar para a máquina, e um gate de CI que o usava passava antes
  # de o apply recusar. Continua a pôr o manifesto no stdout; a recusa vai para
  # o stderr.
  check "conflito de posse: o apply --dry-run da outra stack sai 5 com DX-5601, o manifesto no stdout" 5 bash -c "
    out=\$('$BIN' stack apply -f '$CFWORK/a.yaml' --name '$CSB' --dry-run 2>'$CFWORK/dry.err'); rc=\$?
    cat '$CFWORK/dry.err'
    [[ \$rc == 5 ]] && { ! grep -q DX-5601 '$CFWORK/dry.err' || ! grep -q '^kind: Volume' <<<\"\$out\"; } && exit 99
    exit \$rc"
  # O `manifest render` é a vista sem a máquina: o mesmo manifesto, com o
  # conflito por baixo, sai 0. Foi implementado como `apply --dry-run` e herdou
  # a recusa quando o dry-run passou a planear; o cli-tester apanhou-o.
  check "conflito de posse: o manifest render do mesmo manifesto sai 0, com o manifesto" ok bash -c "
    out=\$('$BIN' manifest render -f '$CFWORK/a.yaml') || exit \$?
    grep -q '^kind: Volume' <<<\"\$out\" || { echo 'sem o manifesto no stdout'; exit 99; }"
  check "…e nada mudou: os dois registos iguais, a stk-b sem revisão, a stk-a sem deriva" ok bash -c "
    s=\$(cd '$CROOT' && sha256sum 'volumes/$CFV/meta.json' 'networks/$CFN') || { echo 'um registo desapareceu'; exit 1; }
    [[ -n '$CF_A' && \"\$s\" == '$CF_A' ]] || { echo 'o registo mudou'; exit 1; }
    n=\$('$BIN' stack history --name '$CSB' -o json | python3 -c 'import json,sys; print(len(json.load(sys.stdin)))')
    [[ \$n =~ ^[0-9]+\$ && \$n == '$CF_AREVS' ]] || { echo \"a $CSB passou de '$CF_AREVS' para '\$n' revisões\"; exit 1; }
    '$BIN' stack plan -f '$CFWORK/a.yaml' --name '$CSA' --detailed-exitcode"
  if check_then "conflito de posse: a stk-b aplica um volume seu" ok \
       "$BIN" stack apply -f "$CFWORK/b.yaml" --name "$CSB"; then
    CF_B=$(cf_sums "volumes/$CFB/meta.json"); CF_BREVS=$(cf_revs "$CSB")
    # Um plano com um conflito E uma alteração saía 2, «há o que aplicar», e o
    # apply não aplicaria nada. O conflito ganha ao 2.
    check "conflito + alteração: o stack plan --detailed-exitcode sai 5, não 2" 5 bash -c "
      out=\$('$BIN' stack plan -f '$CFWORK/misto.yaml' --name '$CSB' --detailed-exitcode 2>&1); rc=\$?
      printf '%s\n' \"\$out\"
      [[ \$rc == 5 ]] && ! grep -q DX-5601 <<<\"\$out\" && exit 99
      exit \$rc"
    # ...e sem conflito, a mesma alteração continua a ser 2. O 2 é também o rc
    # de um erro de uso do clap (`docs/cli-stability.md`): só conta se a saída
    # for um plano que nomeia o volume, e não a ajuda de uma flag recusada.
    check "…e a mesma alteração sem conflito continua 2" 2 bash -c "
      out=\$('$BIN' stack plan -f '$CFWORK/b2.yaml' --name '$CSB' --detailed-exitcode 2>&1); rc=\$?
      printf '%s\n' \"\$out\"
      [[ \$rc == 2 ]] && { grep -q 'Usage:' <<<\"\$out\" || ! grep -q 'Volume/$CFB' <<<\"\$out\"; } && exit 99
      exit \$rc"
    # O conflito ganha a um Replace por autorizar: passar `--replace` não o
    # desbloquearia, e «argumento inválido» mandava o operador atrás dele.
    check "conflito + Replace por autorizar continua 5 com DX-5601" 5 bash -c "
      out=\$('$BIN' stack apply -f '$CFWORK/misto.yaml' --name '$CSB' 2>&1); rc=\$?
      printf '%s\n' \"\$out\"
      [[ \$rc == 5 ]] && ! grep -q DX-5601 <<<\"\$out\" && exit 99
      exit \$rc"
    # ...e sem conflito, o Replace por autorizar continua a ser o que era.
    check "um Replace por autorizar sozinho continua 1 com DX-1000" 1 bash -c "
      out=\$('$BIN' stack apply -f '$CFWORK/b2.yaml' --name '$CSB' 2>&1); rc=\$?
      printf '%s\n' \"\$out\"
      [[ \$rc == 1 ]] && { ! grep -q DX-1000 <<<\"\$out\" || grep -q DX-5601 <<<\"\$out\"; } && exit 99
      exit \$rc"
    check "…e nada mudou: os três registos iguais, a stk-b sem revisão nova" ok bash -c "
      s=\$(cd '$CROOT' && sha256sum 'volumes/$CFV/meta.json' 'networks/$CFN' 'volumes/$CFB/meta.json') || { echo 'um registo desapareceu'; exit 1; }
      [[ -n '$CF_B' && \"\$s\" == '$CF_B' ]] || { echo 'o registo mudou'; exit 1; }
      n=\$('$BIN' stack history --name '$CSB' -o json | python3 -c 'import json,sys; print(len(json.load(sys.stdin)))')
      [[ \$n =~ ^[0-9]+\$ && \$n == '$CF_BREVS' ]] || { echo \"a $CSB passou de '$CF_BREVS' para '\$n' revisões\"; exit 1; }"
    "$BIN" stack destroy -f "$CFWORK/b.yaml" --name "$CSB" >/dev/null 2>&1
  fi
  "$BIN" stack destroy -f "$CFWORK/a.yaml" --name "$CSA" >/dev/null 2>&1
  check "conflito de posse: as duas stacks levaram o que criaram" ok bash -c "
    '$BIN' volume inspect '$CFV' >/dev/null 2>&1; [ \$? -eq 4 ] || exit 1
    '$BIN' network inspect '$CFN' >/dev/null 2>&1; [ \$? -eq 4 ] || exit 1
    '$BIN' volume inspect '$CFB' >/dev/null 2>&1; [ \$? -eq 4 ]"
fi
# Só apanha alguma coisa quando um check acima chumbou (um apply que adoptou o
# recurso alheio deixa-o carimbado com a stack errada, e o destroy dessa não o
# leva): o FAIL já está no relatório, e o root não fica com restos.
"$BIN" volume rm "$CFV" >/dev/null 2>&1
"$BIN" network rm "$CFN" >/dev/null 2>&1
"$BIN" volume rm "$CFB" >/dev/null 2>&1
rm -rf "$CFWORK"

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
# O `plan` responde pelo `apply` com as MESMAS flags. Um `apply` sem `--prune`
# não remove o hv2, por isso um `plan` sem `--prune` lista-o como MANTIDO e não
# o conta: até aqui dava 2 («1 to remove») para sempre, e um portão de deriva
# sem poda nunca ficava verde. O `so1.yaml` é a revisão 1 ao lado do
# `hist.yaml`: o nome da stack vem do directório, é a mesma stack.
cat >"$HWORK/so1.yaml" <<YAML
apiVersion: delonix.io/v1
kind: Volume
metadata:
  name: hv-$PFX
spec: {}
YAML
check "stack plan sem --prune: o que saiu do manifesto fica MANTIDO e o --detailed-exitcode é 0" 0 bash -c "
  out=\$('$BIN' stack plan -f '$HWORK/so1.yaml' --detailed-exitcode 2>&1); rc=\$?
  printf '%s\n' \"\$out\"
  [[ \$rc == 0 ]] && ! grep -q 'Volume/hv2-$PFX .*kept' <<<\"\$out\" && exit 99
  exit \$rc"
# No JSON: o `action` continua `delete`, o `changed` é `false`, a `reason` nomeia
# o `--prune`, e o `planDigest` é o MESMO do plano com `--prune` — senão um
# `apply --plan-digest` recusaria um dos dois como desactualizado.
check "…no -o json: delete, changed false, e o mesmo planDigest que com --prune" ok bash -c "
  a=\$('$BIN' stack plan -f '$HWORK/so1.yaml' -o json) || exit 1
  b=\$('$BIN' stack plan -f '$HWORK/so1.yaml' -o json --prune) || exit 1
  python3 -c 'import json,sys
f=lambda t: [c for c in json.loads(t) if c[\"name\"]==sys.argv[3]][0]
a,b=f(sys.argv[1]),f(sys.argv[2])
ok=(a[\"action\"]==b[\"action\"]==\"delete\" and a[\"changed\"] is False and b[\"changed\"] is True
    and \"--prune\" in a[\"reason\"] and a[\"planDigest\"]==b[\"planDigest\"])
sys.exit(0 if ok else 1)' \"\$a\" \"\$b\" 'hv2-$PFX'"
# Com `--prune` o plano é o do `apply --prune`: a remoção conta. O 2 é também o
# rc de um erro de uso do clap: só conta se a saída for um plano que nomeia o hv2.
check "stack plan --prune: a remoção conta, o --detailed-exitcode é 2" 2 bash -c "
  out=\$('$BIN' stack plan -f '$HWORK/so1.yaml' --prune --detailed-exitcode 2>&1); rc=\$?
  printf '%s\n' \"\$out\"
  [[ \$rc == 2 ]] && { grep -q 'Usage:' <<<\"\$out\" || ! grep -q '1 to remove' <<<\"\$out\"; } && exit 99
  exit \$rc"
# Um `apply` sem `--prune` não remove o hv2, por isso não pede nada à máquina:
# não grava revisão. Gravava uma por cada corrida, com `delete` no resumo — três
# applies levavam a stack de N a N+3 revisões a dizer que o hv2 tinha saído, com
# o hv2 lá. Com retenção de 20, um ciclo de CI ou de GitOps sem poda empurrava o
# histórico útil para fora.
check "três applies sem --prune com um recurso que saiu do manifesto não gastam revisões" ok bash -c "
  n() { '$BIN' stack history -f '$HWORK/so1.yaml' -o json | python3 -c 'import json,sys; print(len(json.load(sys.stdin)))'; }
  before=\$(n); for i in 1 2 3; do '$BIN' stack apply -f '$HWORK/so1.yaml' >/dev/null 2>&1 || exit 1; done
  after=\$(n); echo \"revisões: \$before -> \$after\"
  '$BIN' volume inspect 'hv2-$PFX' >/dev/null || { echo 'o hv2 desapareceu sem --prune'; exit 1; }
  [[ \$before == \$after ]]"
# A última revisão continua a ser a do `rollback --to 1` sem `--prune`, que mudou
# o hv e deixou o hv2: o resumo diz `kept` para o hv2, nunca `delete`.
check "…e a revisão do rollback sem --prune diz kept, não delete" ok bash -c "
  '$BIN' stack history -f '$HWORK/so1.yaml' -o json | python3 -c 'import json,sys
r=json.load(sys.stdin)[-1]; s=r.get(\"summary\") or {}
print(r.get(\"number\"), r.get(\"rollback_of\"), s)
sys.exit(0 if s.get(\"kept\")==1 and not s.get(\"delete\") else 1)'"
# Só o stderr, e a frase que só o aviso tem: desde que o plano escreve «kept,
# `--prune` removes it» no stdout, procurar `--prune` em `2>&1` passava mesmo
# sem o aviso.
check "…e o rollback avisa que é preciso --prune para o levar" ok \
  bash -c "'$BIN' stack rollback --to 1 -f '$HWORK/hist.yaml' --dry-run 2>&1 >/dev/null | grep -q 'does not delete on its own'"
check "rollback --prune leva-o" ok \
  "$BIN" stack rollback --to 1 -f "$HWORK/hist.yaml" --prune
check "…e agora já não está" 4 "$BIN" volume inspect "hv2-$PFX"
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

e2e_rm "$THC"
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

e2e_rm "$TZ"
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
e2e_rm "$TALW"
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
e2e_rm "$TSTOP" "$TWAIT" "$THC" "$TZ" "$TALW"

# --- o slirp de um container acaba com ele, e não segura o arranque ---------
#
# Um `slirp4netns` não sai quando o processo que serve morre, e era lançado com
# todos os descritores de quem o lançava. Os onze cenários — e o que cada um
# media antes da correcção — estão no cabeçalho do `e2e_slirp_lifecycle.sh`,
# que corre sozinho contra qualquer binário. 77 = o host não deixa medir.
declare -A SLIRP_CHECKS=(
  [fds]="slirp: não herda os pipes do arranque, nem o comando do container"
  [exit]="slirp: um container que sai sozinho leva o slirp, a porta e o shim de logs"
  [start]="slirp: container start não pendura com o slirp antigo na porta (supervisor morto)"
  [listed]="slirp: rm de um container já saído e listado ceifa o slirp dele"
  [stats]="slirp: o stats que regista a saída de um container solta o slirp dele"
  [kind]="slirp: o cluster ls que regista a saída de um nó solta o slirp dele"
  [hang]="slirp: container start devolve, com a causa, quando o slirp que lançou não responde"
  [zombies]="slirp: um --restart always que cai sempre não junta zombies no supervisor"
  [stopgaveup]="slirp: um stop que desiste (DX-8101) solta a porta e o slirp logo"
  [infra]="rede: o pin e o plano de controlo não ficam com descritores de quem os arrancou"
  [execfail]="slirp: a run -d whose command does not exist releases its published port"
)
for _sc in fds exit start listed stats kind hang zombies stopgaveup execfail infra; do
  _out=$(bash "$(dirname "$0")/e2e_slirp_lifecycle.sh" "$_sc" "$BIN" "$IMG" "sl$PFX" 2>&1); _rc=$?
  if [[ $_rc -eq 77 ]]; then
    skip "${SLIRP_CHECKS[$_sc]}" "$_out"
  else
    check "${SLIRP_CHECKS[$_sc]}" ok bash -c 'printf "%s\n" "$1"; exit "$2"' _ "$_out" "$_rc"
  fi
done

# --- um rm -f que desiste não deixa o --restart ressuscitar o container -----
# Ver o cabeçalho do `e2e_rm_force_gave_up.sh` (como se segura a saída do
# PID 1 sem disco saturado). 77 = o host não deixa medir.
_out=$(bash "$(dirname "$0")/e2e_rm_force_gave_up.sh" "$BIN" "$IMG" "rf$PFX" 2>&1); _rc=$?
if [[ $_rc -eq 77 ]]; then
  skip "rm -f que desiste (DX-8101) não deixa o supervisor ressuscitar o container" "$_out"
else
  check "rm -f que desiste (DX-8101) não deixa o supervisor ressuscitar o container" ok \
    bash -c 'printf "%s\n" "$1"; exit "$2"' _ "$_out" "$_rc"
fi

# --- the battery's own teardown, proved against a real DX-8101 ---------------
# `e2e_rm` repeats a `rm -f` that answers DX-8101 and says when it gives up;
# without it a loaded disk turned one slow exit into a cascade of FAILs in the
# sections after it. See the header of `e2e_rm_teardown.sh`. 77 = the host does
# not let it measure. `converges` and `gives-up` each wait out a 30 s `rm -f`.
declare -A RMT_CHECKS=(
  [paths]="teardown: DX-8101 is repeated, class 4 is silent, any other failure is said"
  [converges]="teardown: a held exit that answers DX-8101 is waited out and removed"
  [gives-up]="teardown: a DX-8101 past the deadline is given up and said, the record kept"
)
for _sc in paths converges gives-up; do
  _out=$(bash "$(dirname "$0")/e2e_rm_teardown.sh" "$_sc" "$BIN" "$IMG" "rt$PFX" 2>&1); _rc=$?
  if [[ $_rc -eq 77 ]]; then
    skip "${RMT_CHECKS[$_sc]}" "$_out"
  else
    check "${RMT_CHECKS[$_sc]}" ok bash -c 'printf "%s\n" "$1"; exit "$2"' _ "$_out" "$_rc"
  fi
done

# --- a container past the mount ceiling is never entered (issue #793) -------
# A start whose init has not said its mounts are done within the ceiling is
# published as running but NOT mounted: exec/cp refuse until the proof arrives,
# and it comes up on its own after. See the header of `e2e_mount_ceiling.sh`
# (how the init is held without saturating the disk). 77 = the host does not
# let it measure. The scenario waits out the full 60 s ceiling.
MC_CHECK="mount ceiling: a container not seen mounted is never entered, and comes up once it is"
_out=$(bash "$(dirname "$0")/e2e_mount_ceiling.sh" "$BIN" "$IMG" "mc$PFX" 2>&1); _rc=$?
if [[ $_rc -eq 77 ]]; then
  skip "$MC_CHECK" "$_out"
else
  check "$MC_CHECK" ok bash -c 'printf "%s\n" "$1"; exit "$2"' _ "$_out" "$_rc"
fi

# --- one mount point under a privileged container's /sys, ONE mount --------
# ADR-0078 pendency #1, measured on a 2-node kubeadm cluster: the Cilium agent
# asks for `hostPath /sys/fs/bpf` and died with `multiple mount points detected
# at /sys/fs/bpf` on a node whose host has exactly ONE bpffs. The engine hands
# a privileged container its `/sys` (a fresh `sysfs`, or a recursive bind of
# the host's when the user ns does not own the netns) AFTER injecting the
# volumes, so the explicit bind ended up UNDER it — invisible with the fresh
# sysfs, and duplicated at the same point with the recursive bind, which
# carries the host's own bpffs along.
#
# Both invariants are read the way the agent reads them (field 5 of
# `mountinfo`, the mount point itself) and hold whichever `/sys` branch ran:
# exactly ONE mount at the point, a CHILD of `/sys` — on top of it, which is
# what makes it the mount every lookup resolves to — and carrying the
# propagation that was asked for. No bpffs on the host, nothing to measure.
BPFFS_ONE="privileged: a -v under /sys is mounted ONCE, on top of /sys"
BPFFS_PROP="privileged: that mount keeps the rshared it asked for"
if awk '$5=="/sys/fs/bpf"' /proc/self/mountinfo | grep -q .; then
  # The container only `cat`s its own mountinfo; the counting is done here, so
  # the assertion does not depend on what the image's shell brings.
  _bpfmi=$("$BIN" container run --rm --privileged -v /sys/fs/bpf:/sys/fs/bpf:rshared \
    "$IMG" cat /proc/self/mountinfo 2>/dev/null)
  _bpfv=$(printf '%s\n' "$_bpfmi" | awk '
    $5 == "/sys"        { sysid = $1 }
    $5 == "/sys/fs/bpf" { n++; parent = $2; prop = ($0 ~ / shared:/) ? "rshared" : "private" }
    END { printf "%d %s %s\n", n + 0, (n == 1 && parent == sysid) ? "on-top" : "buried", prop }')
  check "$BPFFS_ONE" ok bash -c \
    '[ "${1% *}" = "1 on-top" ] || { printf "mountinfo said: %s\n" "$1"; exit 1; }' _ "$_bpfv"
  check "$BPFFS_PROP" ok bash -c \
    '[ "${1##* }" = rshared ] || { printf "mountinfo said: %s\n" "$1"; exit 1; }' _ "$_bpfv"
else
  skip "$BPFFS_ONE" "este host não tem bpffs montado em /sys/fs/bpf"
  skip "$BPFFS_PROP" "este host não tem bpffs montado em /sys/fs/bpf"
fi

# --- the same invariant for the HOT-PLUG path -----------------------------
# The check above orders `setup_rootfs`; this one covers `mount_live`, which
# reaches the same mount point on a container that is ALREADY running. The
# only collision guard on that path compares against the mounts the engine
# RECORDED, and a privileged container's `/sys` (plus the `cgroup2` under it)
# is not among them — so `--volume-add /sys/fs/bpf:/sys/fs/bpf` used to stack a
# second mount on the bpffs the engine had already delivered, rebuilding the
# exact condition the Cilium agent refuses to start on, after start-up.
# Measured before the fix: 2 entries at the point; after: 1.
HOTPLUG_ONE="hot-plug: --volume-add onto the engine's own /sys/fs/bpf stays ONE mount"
if awk '$5=="/sys/fs/bpf"' /proc/self/mountinfo | grep -q .; then
  _hp="dlx-e2e-hotplug-$$"
  "$BIN" container run -d --privileged --name "$_hp" "$IMG" sleep 60 >/dev/null 2>&1
  "$BIN" container update "$_hp" --volume-add /sys/fs/bpf:/sys/fs/bpf >/dev/null 2>&1
  # Counted here, not in the container, for the reason the block above gives.
  _hpn=$("$BIN" container exec "$_hp" cat /proc/self/mountinfo 2>/dev/null |
    awk '$5=="/sys/fs/bpf"{n++} END{print n+0}')
  "$BIN" container rm -f "$_hp" >/dev/null 2>&1 || true
  check "$HOTPLUG_ONE" ok bash -c \
    '[ "$1" = 1 ] || { printf "mountinfo listed %s mounts at /sys/fs/bpf\n" "$1"; exit 1; }' _ "$_hpn"
else
  skip "$HOTPLUG_ONE" "this host has no bpffs mounted at /sys/fs/bpf"
fi

# --- a state root written by the previous release opens unchanged ----------
# Plan 65 F0.6. DELONIX_UPGRADE_FROM names a directory with the PUBLISHED
# binaries of the previous release (checked against SHA256SUMS and the
# repository's minisign key); without it there is nothing to compare with,
# and the check says so. See the header of `e2e_state_upgrade.sh`.
UPG_CHECK="upgrade: um root escrito pela release anterior abre igual (plan sem diferenças, dados, segredo, pid)"
if [[ -z "${DELONIX_UPGRADE_FROM:-}" ]]; then
  skip "$UPG_CHECK" "DELONIX_UPGRADE_FROM não está definido (diretório com os binários publicados da release anterior)"
else
  _out=$(bash "$(dirname "$0")/e2e_state_upgrade.sh" "$DELONIX_UPGRADE_FROM" "$(dirname "$BIN")" "$OUT/upg" "$IMG" 2>&1); _rc=$?
  if [[ $_rc -eq 77 ]]; then
    skip "$UPG_CHECK" "$_out"
  else
    check "$UPG_CHECK" ok bash -c 'printf "%s\n" "$1"; exit "$2"' _ "$_out" "$_rc"
  fi
fi

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
# The scaffold itself is a judged invocation (a setup line that ends in a
# silent `if` is invisible to the coverage ratchet); what it must have produced
# is judged by the two checks below.
if check_then "vm init: gera o projecto" ok "$BIN" vm init "$VMINIT" --name "v$PFX" \
   && check_then "vm init: o manifesto gerado não está vazio" ok test -s "$VMINIT/delonix-manifest.yaml"; then
  check "vm init: o projecto que gera valida-se" ok \
    "$BIN" stack validate -f "$VMINIT/delonix-manifest.yaml"
  check "vm init: gera os três Kinds" ok bash -c "
    for k in Network Volume VirtualMachine; do
      grep -q \"kind: \$k\" '$VMINIT/delonix-manifest.yaml' || { echo \"falta kind: \$k\"; exit 1; }
    done
  "
fi

########################################
section "workload / pod / secret (leitura)"
########################################
check "workload ls" ok "$BIN" workload ls
check "workload ls -o json" ok "$BIN" workload ls -o json
check "workload describe inexistente recusa" 4 "$BIN" workload describe "nao-existe-$PFX"
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
if check_then "pod create de dois membros" ok bash -c \
     "'$BIN' pod create -f '$PODY' 2>'$OUT/pod-$PFX.err' >/dev/null || { cat '$OUT/pod-$PFX.err'; exit 1; }"; then
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
  # ADR-0069 item 1: o lado da LEITURA da firewall de um pod. Um pod é UM alvo
  # (os membros partilham a netns e o endereço, e a política vive no
  # membro-cabeça), e até 2026-10-06 só as MUTAÇÕES o sabiam: `net ingress allow
  # <pod>` funcionava e `net ingress ls <pod>` respondia «no such container»
  # (rc=4), tal como o `describe networkpolicies <pod>/<dir>`. Uma regra que se
  # escreve e não se vê é pior que uma que não se escreve.
  #
  # E o `get networkpolicies` nomeava o MEMBRO onde o operador escreveu o POD:
  # num pod de dois membros imprimia QUATRO linhas, e as duas do membro sem
  # firewall diziam «allow (default), 0 rules» por cima de uma política que
  # existe. Uma listagem que nega uma firewall é pior que nenhuma.
  check "net ingress allow <pod>: a mutação aceita o nome do pod" ok \
    "$BIN" net ingress allow "p$PFX" 8099
  check "... e o ls LÊ pelo mesmo nome, com a regra" ok bash -c "
    out=\$('$BIN' net ingress ls 'p$PFX' 2>&1) || { printf '%s\n' \"\$out\"; exit 1; }
    printf '%s' \"\$out\" | grep -q 'firewall for p$PFX' || { echo 'o ls não nomeia o pod'; exit 1; }
    printf '%s' \"\$out\" | grep -q 8099 || { echo 'a regra que o allow escreveu não aparece'; exit 1; }"
  check "... e o describe genérico também" ok bash -c "
    '$BIN' describe networkpolicies 'p$PFX/ingress' 2>&1 | grep -q 'firewall for p$PFX'"
  check "... e o get conta o pod UMA vez por direcção, com o nome do pod" ok bash -c "
    out=\$('$BIN' get networkpolicies 2>&1)
    printf '%s' \"\$out\" | grep -qE '^p$PFX +ingress' || { echo 'o get não nomeia o pod'; printf '%s\n' \"\$out\"; exit 1; }
    n=\$(printf '%s\n' \"\$out\" | grep -cE '^p$PFX(-| )' || true)
    [ \"\$n\" = 2 ] || { echo \"o pod deu \$n linhas, esperava 2 (uma por direcção)\"; printf '%s\n' \"\$out\"; exit 1; }"
  "$BIN" net ingress clear "p$PFX" >/dev/null 2>&1

  check "describe pod" ok "$BIN" describe pod "p$PFX"
  check "delete pod -f" ok "$BIN" delete pod "p$PFX" -f
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
if check_then "pod com hostPort: pod create" ok "$BIN" pod create -f "$PPY" \
   && check_then "pod com hostPort: a porta responde 200" ok pp_wait 200; then
  "$BIN" container stop -t 1 "pp$PFX-web" >/dev/null 2>&1
  check "o stop de um membro de pod liberta a porta" ok bash -c "[ -z \"\$(ss -tlnH 'sport = :$PPORT')\" ]"
  "$BIN" container start "pp$PFX-web" >/dev/null 2>&1
  check "o start de um membro de pod volta a publicar a porta" ok bash -c \
    "for _ in \$(seq 1 30); do [ \"\$(curl -s -m 5 -o /dev/null -w '%{http_code}' http://127.0.0.1:$PPORT/)\" = 200 ] && exit 0; sleep 0.5; done; exit 1"
  e2e_rm "pp$PFX-web"
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
  e2e_rm "pp$PFX-web"
fi
# A pod that came up without answering is still there: same teardown either way.
e2e_rm "pp$PFX-web"
check "secret ls" ok "$BIN" secret ls
check "secret inspect inexistente recusa" 4 "$BIN" secret inspect "nao-existe-$PFX"

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

# `secret rotate-key` roda a CHAVE-MESTRA do nó: recifra TODOS os segredos e as
# credenciais de túnel do root. Num root partilhado isso mexia nos segredos
# reais do utilizador, por isso só corre isolado — mesma guarda do D5 acima.
#
# ACH-034 (2026-10-08): a rotação não muda valor nenhum, mas punha a VERSÃO de
# cada segredo a 1 — o `save` decidia a versão comparando com o ficheiro em
# disco, que nessa altura já estava selado com a chave ANTIGA; a decifra falhava
# e caía no ramo da primeira gravação. Corrigido com um caminho de gravação
# interno que recebe a versão a manter, deixando a regra do `save` intacta.
if [[ "$E2E_ISOLATED" != 1 || "$DELONIX_ROOT" == "$REAL_ROOT" ]]; then
  skip "secret rotate-key" "estado partilhado — a rotação mexia na chave-mestra real do nó"
else
  SECR="secr-$PFX"
  "$BIN" secret create "$SECR" --from-literal k=a >/dev/null 2>&1
  for v in b c d; do "$BIN" secret set "$SECR" "k=$v" >/dev/null 2>&1; done
  check "secret: a versão conta as mudanças de valor (4 gravações distintas)" ok bash -c \
    "'$BIN' secret inspect '$SECR' 2>&1 | grep -qx 'Version: 4'"
  check "secret rotate-key: roda a chave-mestra" ok bash -c \
    "'$BIN' secret rotate-key 2>&1 | grep -q 'master key rotated'"
  check "secret rotate-key: a versão NÃO recua (os valores não mudaram)" ok bash -c \
    "'$BIN' secret inspect '$SECR' 2>&1 | grep -qx 'Version: 4'"
  check "secret rotate-key: recifra tudo e os valores em claro continuam os MESMOS" ok bash -c \
    "'$BIN' secret inspect '$SECR' --reveal 2>&1 | grep -q 'k=d'"
  check "secret rotate-key: e uma mudança DEPOIS da rotação segue de 4 para 5" ok bash -c \
    "'$BIN' secret set '$SECR' k=e >/dev/null 2>&1
     '$BIN' secret inspect '$SECR' 2>&1 | grep -qx 'Version: 5'"
  "$BIN" secret rm "$SECR" >/dev/null 2>&1
fi

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
# `describe`, e não `vm inspect`: este verbo deixou de existir, e o check
# passava por «unrecognized subcommand» (rc=2), nunca por a VM não existir.
check "vm create --require: nenhum registo de VM ficou para trás (4)" 4 "$BIN" describe virtualmachines "vm-$PFX-req"
# The auto-selection filters by the requirement (ADR-0050 D6). A cloud image
# PREFERS libvirt, and asking for it by name when it lacks the requirement
# refused the VM with Cloud Hypervisor installed and able to serve it.
if command -v cloud-hypervisor >/dev/null && command -v virsh >/dev/null; then
  check "vm create --require sem --backend: um requisito que só o CH cumpre não é recusado pelo libvirt" ok bash -c "! \"\$0\" vm create vm-$PFX-req --disk /nao/existe.qcow2 --require vm.namespace-isolation 2>&1 | grep -q \"'libvirt' backend does not support\"" "$BIN"
  check "vm create --require sem --backend: nenhum candidato serve, a recusa nomeia os dois (69)" 69 "$BIN" vm create "vm-$PFX-req" --disk /nao/existe.qcow2 --require vm.namespace-isolation --require vm.snapshot.memory
else
  skip "vm create --require sem --backend (auto-selecção)" "precisa de cloud-hypervisor e libvirt instalados"
fi
# ADR-0069 D6 para convidados: uma VM que uma `NetworkPolicy` com `scope: vm`
# governa nasce FECHADA, e só um backend com firewall PRÓPRIA o consegue fazer
# antes de o convidado ter CPU. Os backends locais não têm nenhuma, por isso o
# create é RECUSADO pelo nome — nunca uma VM de pé e ABERTA debaixo de uma
# política que diz fechada, que é o defeito que o hold existe para evitar.
# DX-1501 é a classe (invalid): o pedido é que não se pode cumprir aqui.
if command -v qemu-img >/dev/null; then
  PHW="$WORK/policy-hold-vm"; mkdir -p "$PHW"; PHVM="phvm-$PFX"
  qemu-img create -f qcow2 "$PHW/d.qcow2" 64M >/dev/null 2>&1
  cat >"$PHW/delonix-manifest.yaml" <<YAML
apiVersion: compute.delonix.io/v1alpha1
kind: VirtualMachine
metadata:
  name: $PHVM
spec:
  disk: $PHW/d.qcow2
  memory: 64M
  vcpus: 1
---
apiVersion: networking.delonix.io/v1alpha1
kind: NetworkPolicy
metadata:
  name: $PHVM-p
spec:
  target: $PHVM
  scope: vm
  direction: ingress
  defaultPolicy: deny
  rules: []
YAML
  check "scope: vm num backend sem firewall própria recusa, em vez de criar a VM aberta" ok bash -c "
    out=\$('$BIN' stack apply -f '$PHW/delonix-manifest.yaml' 2>&1) && { echo 'o apply devia ter falhado'; printf '%s\n' \"\$out\"; exit 1; }
    printf '%s\n' \"\$out\"
    printf '%s' \"\$out\" | grep -q 'cannot create a VM closed' || { echo 'a recusa não nomeia o motivo'; exit 1; }
    printf '%s' \"\$out\" | grep -q 'proxmox' || { echo 'a recusa não diz onde o scope: vm serve'; exit 1; }"
  check "... e nenhuma VM ficou para trás (4)" 4 "$BIN" describe virtualmachines "$PHVM"
else
  skip "scope: vm num backend local" "precisa de qemu-img para o disco de teste"
fi

# `vm resize`: tudo o que se pode recusar recusa-se ANTES de tocar num backend,
# por isso estas classes medem-se sem hipervisor nenhum.
check "vm resize sem --vcpus nem --memory recusa (1)" 1 "$BIN" vm resize "vm-$PFX-nada"
check "vm resize de uma VM inexistente diz 4" 4 "$BIN" vm resize "vm-$PFX-nada" --vcpus 2
# `vm move --node` (ADR-0053): o destino vazio e a VM inexistente recusam-se
# antes de qualquer backend.
check "vm move com destino vazio recusa (1)" 1 "$BIN" vm move "vm-$PFX-nada" --node ""
check "vm move de uma VM inexistente diz 4" 4 "$BIN" vm move "vm-$PFX-nada" --node pve2
# `--target-storage` só faz sentido com `--with-local-disks`, e um vazio não é
# um storage: as duas recusas são do motor, antes de tocar no backend (1538).
check "vm move --target-storage sem --with-local-disks recusa (1)" 1 \
  "$BIN" vm move "vm-$PFX-nada" --node pve2 --target-storage nfs-lab
check "vm move --target-storage vazio recusa (1)" 1 \
  "$BIN" vm move "vm-$PFX-nada" --node pve2 --with-local-disks --target-storage ""
check "vm cloud-init sem nada para mudar recusa (1)" 1 "$BIN" vm cloud-init "vm-$PFX-nada"
check "vm cloud-init com hostname inválido recusa (1)" 1 "$BIN" vm cloud-init "vm-$PFX-nada" --hostname=a.b
check "vm cloud-init de uma VM inexistente diz 4" 4 "$BIN" vm cloud-init "vm-$PFX-nada" --hostname web-1

# ADR-0054: the node's providers file and the default provider. Own root and
# own providers file for every call: these checks write a machine-wide default,
# and a providers file on this host (`/etc/delonix/providers.yaml`, or one under
# `~/.config`) would otherwise decide the outcome — DELONIX_PROVIDERS_CONFIG wins
# over both. `pve.invalid` never resolves (RFC 6761), so nothing is contacted.
ADR54_ROOT="$OUT/adr54-root"; mkdir -p "$ADR54_ROOT"
ADR54_EMPTY="$OUT/adr54-empty.yaml"
printf 'apiVersion: config.delonix.io/v1\n' >"$ADR54_EMPTY"
adr54_env() { env -u DELONIX_PROXMOX_URL -u DELONIX_PROXMOX_SECRET -u DELONIX_PROXMOX_TOKEN_ID \
  -u DELONIX_PROXMOX_TOKEN -u DELONIX_PROXMOX_USER -u DELONIX_PROXMOX_PASSWORD -u DELONIX_VM_BACKEND \
  DELONIX_ROOT="$ADR54_ROOT" "$@"; }
adr54_px() { adr54_env DELONIX_PROVIDERS_CONFIG="$ADR54_EMPTY" DELONIX_PROXMOX_URL=https://pve.invalid:8006 \
  DELONIX_PROXMOX_NODE=pve DELONIX_PROXMOX_TOKEN_ID='e2e@pve!t' DELONIX_PROXMOX_TOKEN=x "$BIN" "$@"; }
adr54_bare() { adr54_env DELONIX_PROVIDERS_CONFIG="$ADR54_EMPTY" "$BIN" "$@"; }
adr54_file() { local f="$1"; shift; adr54_env DELONIX_PROVIDERS_CONFIG="$f" "$BIN" "$@"; }
adr54_fns="$(declare -f adr54_env adr54_px adr54_bare adr54_file); BIN='$BIN' ADR54_ROOT='$ADR54_ROOT' ADR54_EMPTY='$ADR54_EMPTY'"

# §3, the legacy default: set where the target exists, read where it does not.
# Written directly: `--set` writes the providers file since slice 3, and this
# case is the per-root legacy default the engine still reads as a fallback.
printf 'proxmox\n' >"$ADR54_ROOT/vm-default-backend"
check "o default antigo (por root) está escrito" ok test -s "$ADR54_ROOT/vm-default-backend"
# The control: with the target present the request does go to the node — so
# the two checks after it measure the missing target and nothing else.
check "vm create com o alvo no ambiente vai ao nó Proxmox" ok bash -c \
  "$adr54_fns; adr54_px vm create vm-$PFX-adr54 --disk /nao/existe.qcow2 2>&1 | grep -q 'pve.invalid'"
check "vm default-backend sem o alvo diz proxmox, não 'none'" ok bash -c \
  "$adr54_fns; ! adr54_bare vm default-backend 2>&1 | grep -q '^none'"
# Was an xfail until slice 2: the default used to be dropped and the VM went
# to a LOCAL hypervisor. Now it is refused in the unavailable class, naming it.
check "vm create sem o alvo recusa (69) em vez de cair no local" 69 adr54_bare vm create "vm-$PFX-adr54" --disk /nao/existe.qcow2
check "vm create sem o alvo: a recusa nomeia o proxmox e não é o disco local" ok bash -c \
  "$adr54_fns; out=\$(adr54_bare vm create vm-$PFX-adr54 --disk /nao/existe.qcow2 2>&1); echo \"\$out\" | grep -qi proxmox && ! echo \"\$out\" | grep -q 'no such VM image'"
adr54_bare vm default-backend --clear >/dev/null 2>&1 || true

# D1/D3: the default and the target come from the FILE, with no DELONIX_PROXMOX_*
# anywhere in the environment — the node decides, the caller names nothing.
ADR54_TOKEN="$OUT/adr54-token"; printf 'x' >"$ADR54_TOKEN"; chmod 600 "$ADR54_TOKEN"
ADR54_FULL="$OUT/adr54-full.yaml"
cat >"$ADR54_FULL" <<YAML
apiVersion: config.delonix.io/v1
defaultProvider: proxmox
providers:
  - type: libvirt
  - type: proxmox
    url: https://pve.invalid:8006
    node: pve
    auth:
      tokenId: 'e2e@pve!t'
      tokenSecretFile: $ADR54_TOKEN
YAML
check "providers.yaml: o default e o alvo do ficheiro levam o create ao nó" ok bash -c \
  "$adr54_fns; adr54_file '$ADR54_FULL' vm create vm-$PFX-adr54 --disk /nao/existe.qcow2 2>&1 | grep -q 'pve.invalid'"
check "providers.yaml: vm default-backend diz proxmox" ok bash -c \
  "$adr54_fns; adr54_file '$ADR54_FULL' vm default-backend 2>/dev/null | grep -qx proxmox"
ADR54_NOPX="$OUT/adr54-nopx.yaml"
printf 'apiVersion: config.delonix.io/v1\ndefaultProvider: proxmox\nproviders:\n  - type: libvirt\n' >"$ADR54_NOPX"
check "providers.yaml: default sem entrada proxmox recusa (69), nunca local" 69 adr54_file "$ADR54_NOPX" vm create "vm-$PFX-adr54" --disk /nao/existe.qcow2
ADR54_INLINE="$OUT/adr54-inline.yaml"
printf 'apiVersion: config.delonix.io/v1\ndefaultProvider: proxmox\nproviders:\n  - type: proxmox\n    url: https://pve.invalid:8006\n    node: pve\n    auth:\n      tokenId: a@pve!t\n      tokenSecret: plain\n' >"$ADR54_INLINE"
check "providers.yaml: um segredo inline é recusado e o create não corre" 69 adr54_file "$ADR54_INLINE" vm create "vm-$PFX-adr54" --disk /nao/existe.qcow2
check "providers.yaml: a recusa do segredo inline nomeia a forma certa" ok bash -c \
  "$adr54_fns; adr54_file '$ADR54_INLINE' vm create vm-$PFX-adr54 --disk /nao/existe.qcow2 2>&1 | grep -q tokenSecretFile"
check "providers.yaml: DELONIX_PROVIDERS_CONFIG para um ficheiro inexistente recusa" fail adr54_file "$OUT/adr54-nao-existe.yaml" vm create "vm-$PFX-adr54" --disk /nao/existe.qcow2
check "providers.yaml: um ficheiro inválido não impede comandos sem VM" ok adr54_file "$ADR54_INLINE" container ls
check "vm create com ficheiro recusado não deixou registo" fail adr54_bare vm inspect "vm-$PFX-adr54"

# Slice 3: `provider config show|validate`, and `--set` editing the file.
check "provider config validate: o ficheiro completo é válido" ok adr54_env "$BIN" provider config validate -f "$ADR54_FULL"
check "provider config validate: default sem entrada recusa (1)" 1 adr54_env "$BIN" provider config validate -f "$ADR54_NOPX"
check "provider config validate: segredo inline recusa (1)" 1 adr54_env "$BIN" provider config validate -f "$ADR54_INLINE"
chmod 644 "$ADR54_TOKEN"
check "provider config validate: token legível por outros recusa" 1 adr54_env "$BIN" provider config validate -f "$ADR54_FULL"
chmod 600 "$ADR54_TOKEN"
printf 'SEGREDO-e2e-%s' "$PFX" >"$ADR54_TOKEN"
check "provider config show: nomeia o ficheiro, o default e o proxmox" ok bash -c \
  "$adr54_fns; out=\$(adr54_file '$ADR54_FULL' provider config show 2>&1); echo \"\$out\" | grep -q '$ADR54_FULL' && echo \"\$out\" | grep -q 'proxmox (file)' && echo \"\$out\" | grep -q 'pve.invalid'"
check "provider config show: o valor do token nunca aparece (tabela e json)" ok bash -c \
  "$adr54_fns; ! { adr54_file '$ADR54_FULL' provider config show; adr54_file '$ADR54_FULL' provider config show -o json; } 2>&1 | grep -q 'SEGREDO-e2e'"
ADR54_SET="$OUT/adr54-set.yaml"
printf '# kept by --set\napiVersion: config.delonix.io/v1\nproviders:\n  - type: libvirt\n' >"$ADR54_SET"
# The legacy default present again, so the check below measures the move.
printf 'proxmox\n' >"$ADR54_ROOT/vm-default-backend"
check "vm default-backend --set escreve o defaultProvider no ficheiro" ok adr54_file "$ADR54_SET" vm default-backend --set libvirt
check "--set manteve o comentário do operador e escreveu a linha" ok bash -c \
  "grep -qx '# kept by --set' '$ADR54_SET' && grep -qx 'defaultProvider: libvirt' '$ADR54_SET'"
check "--set moveu o default antigo (o ficheiro por root foi retirado)" fail test -e "$ADR54_ROOT/vm-default-backend"
check "vm default-backend --clear tira a linha" ok bash -c \
  "$adr54_fns; adr54_file '$ADR54_SET' vm default-backend --clear >/dev/null && ! grep -q defaultProvider '$ADR54_SET'"
ADR54_SCHEMA="$OUT/adr54-schema.json"
check "provider config schema emite o schema" ok bash -c "$adr54_fns; adr54_env \"\$BIN\" provider config schema >'$ADR54_SCHEMA'"
# Strict like the parser: closed at the top, the version pinned, and the inline
# secret — which the parser refuses by name — never offered as a key.
check "provider config schema: estrito, versão fixa, sem segredo inline" ok python3 -c '
import json, sys
s = json.load(open(sys.argv[1]))
auth = s["$defs"]["ProxmoxAuth"]["properties"]
assert s["additionalProperties"] is False
assert s["properties"]["apiVersion"]["const"] == "config.delonix.io/v1"
assert "tokenSecretFile" in auth and "tokenSecret" not in auth and "password" not in auth
' "$ADR54_SCHEMA"

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

if libvirt_system_usable; then
  SVM="snap-$PFX"; SDISK="$OUT/$SVM.qcow2"
  qemu-img create -f qcow2 "$SDISK" 64M >/dev/null 2>&1
  # `--require vm.snapshot.memory`: the accepted half of ADR-0050 D6 — libvirt
  # marks it usable on a host with qemu:///system, so the requirement passes and
  # the create goes ahead exactly as before (the refusals are in the vm section).
  if check_then "vm create libvirt (snapshot)" ok \
       "$BIN" vm create "$SVM" --disk "$SDISK" --backend libvirt --memory 256M --require vm.snapshot.memory; then
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

    # P4b.3a (ADR-0044, docs/discovery/61): o backup a quente passou a ser um
    # método do backend libvirt. O rc não prova nada — um `blockcommit` sem
    # --top/--base diz «Successfully pivoted» e deixa o domínio a escrever na
    # imagem base partilhada. O que se lê é o domínio VIVO depois do backup.
    BKDIR="$OUT/bk-$SVM"; mkdir -p "$BKDIR"
    check "backup create vm de uma VM libvirt a correr" ok "$BIN" backup create vm "$SVM" --to "$BKDIR"
    check "... o arquivo ficou no destino" ok bash -c "ls '$BKDIR'/vm-$SVM-*.tar.gz >/dev/null 2>&1"
    check "... a VM continua a correr" ok bash -c \
      "[ \"\$(virsh -c qemu:///system domstate '$SVM')\" = running ]"
    check "... e escreve no seu próprio overlay, não no temporário nem na base" ok bash -c \
      "virsh -c qemu:///system domblklist '$SVM' | awk '\$1==\"vda\"{print \$2}' | grep -qx '$SROOT/vms/$SVM.qcow2'"

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

    # `vm resize` (vm.resize.cold, ADR-0049 slice 2). O rc não prova nada: a
    # testemunha é o domínio que o `vm start` seguinte DEFINE, lido do virsh.
    # A recusa com a VM a correr verifica-se pela CLASSE (5) e por o registo não
    # ter mudado — um resize aceite com a VM viva só tomaria efeito no reinício.
    check "vm resize com a VM a correr recusa (5)" 5 "$BIN" vm resize "$SVM" --vcpus 2
    check "vm resize com memória ilegível recusa (1)" 1 "$BIN" vm resize "$SVM" --memory 2GB
    check "vm stop (resize)" ok "$BIN" vm stop "$SVM"
    check "vm resize de uma VM parada" ok "$BIN" vm resize "$SVM" --vcpus 2 --memory 384M
    check "o registo diz 2 vCPU e 384M" ok bash -c \
      "'$BIN' vm ls -A -o json | python3 -c \"import json,sys; sys.exit(0 if any(v['name']=='$SVM' and v['vcpus']==2 and v['memory']=='384M' for v in json.load(sys.stdin)) else 1)\""
    check "vm start depois do resize" ok "$BIN" vm start "$SVM"
    check "o domínio arranca com 2 vCPU" ok bash -c \
      "virsh -c qemu:///system dumpxml '$SVM' | grep -Eq '<vcpu[^>]*>2</vcpu>'"
    check "o domínio arranca com 384 MiB" ok bash -c \
      "virsh -c qemu:///system dumpxml '$SVM' | grep -q \"<memory unit='KiB'>393216</memory>\""
    # `vm move --node` é do Proxmox (ADR-0053): o libvirt não tem cluster e
    # recusa pelo nome, a apontar para o `vm migrate` — nunca um no-op.
    check "vm move no libvirt recusa e nomeia o vm migrate" ok bash -c \
      "out=\$('$BIN' vm move '$SVM' --node pve2 --live 2>&1); rc=\$?; [ \$rc -eq 1 ] && printf '%s' \"\$out\" | grep -q 'vm migrate'"
    "$BIN" delete vm "$SVM" -f >/dev/null 2>&1
  else
    "$BIN" delete vm "$SVM" -f >/dev/null 2>&1
  fi

  rm -f "$SDISK"
else
  skip "vm: snapshot sobrevive a stop/start" "sem virsh/qemu-img/qemu-system-x86_64, ou sem ligação libvirt de sistema"
fi

########################################
section "vm: o restore de snapshot RECUPERA dados de disco (libvirt, guestfish)"
########################################
# Critério #16: CRIAR um snapshot e ver o comando devolver 0 NÃO comprova
# recuperabilidade — o `restore` podia transicionar o estado da VM sem nunca
# repor um único byte do disco. A secção acima prova o CICLO (create/stop/start/
# restore e as transições de estado), mas corre sobre um qcow2 VAZIO: nenhum dado
# é escrito, corrompido e confirmado de volta. Aqui fecha-se esse buraco.
#
# O guest não arranca um OS (disco de 128M sem bootloader), por isso a prova é ao
# nível do BLOCO, via `guestfish` (apliance própria, não precisa do módulo nbd):
#   1. escreve-se um dado conhecido no filesystem do disco (payload = ORIGINAL),
#   2. `vm snapshot create` com a VM PARADA (snapshot de disco interno do qcow2),
#   3. CORROMPE-SE o dado no disco (payload = CORROMPIDO),
#   4. `vm snapshot restore`,
#   5. relê-se o disco — TEM de voltar a ler ORIGINAL.
# É a recuperabilidade de DADOS, não a de um código de saída. A VM tem de estar
# parada nos passos de guestfish porque o QEMU segura o qcow2 em exclusivo quando
# corre; é por isso que a prova de DISCO usa o caminho offline (= vm.snapshot-disk)
# e demonstra, pelo mesmo `snapshot-revert`, que o vm.snapshot-restore repõe dados.
if libvirt_system_usable && command -v guestfish >/dev/null 2>&1; then
  DVM="snapdata-$PFX"; DDISK="$OUT/$DVM.qcow2"
  qemu-img create -f qcow2 "$DDISK" 128M >/dev/null 2>&1
  # Filesystem + payload ORIGINAL, antes de a VM existir.
  guestfish -a "$DDISK" >/dev/null 2>&1 <<'GF'
run
part-disk /dev/sda mbr
mkfs ext4 /dev/sda1
mount /dev/sda1 /
write /payload.txt "DADO-ORIGINAL-v1"
umount /
GF
  if check_then "vm create libvirt (data-recovery)" ok \
       "$BIN" vm create "$DVM" --disk "$DDISK" --backend libvirt --memory 256M --require vm.snapshot.memory; then
    DQCOW="$SROOT/vms/$DVM.qcow2"
    check "vm stop (data-recovery)" ok "$BIN" vm stop "$DVM"
    # vm.snapshot-disk: o snapshot offline CAPTURA o dado para recuperação.
    check "o snapshot de disco captura o dado para recuperacao" ok \
      "$BIN" vm snapshot create "$DVM" sdata
    check "o disco tem o dado ORIGINAL antes de mexer" ok bash -c \
      "guestfish --ro -a '$DQCOW' -m /dev/sda1 cat /payload.txt | grep -qx DADO-ORIGINAL-v1"
    # Corromper o dado no disco, com a VM parada (o qcow2 está livre).
    check "corromper o dado no disco do guest" ok bash -c \
      "guestfish -a '$DQCOW' -m /dev/sda1 write /payload.txt DADO-CORROMPIDO-v2"
    check "o disco ja tem o dado CORROMPIDO" ok bash -c \
      "guestfish --ro -a '$DQCOW' -m /dev/sda1 cat /payload.txt | grep -qx DADO-CORROMPIDO-v2"
    # vm.snapshot-restore: reverter o snapshot.
    check "vm snapshot restore (data-recovery)" ok "$BIN" vm snapshot restore "$DVM" sdata
    # A PROVA: o dado ORIGINAL voltou — recuperabilidade, não só um ficheiro criado.
    check "o restore RECUPERA o dado de disco (nao so um codigo de saida)" ok bash -c \
      "guestfish --ro -a '$DQCOW' -m /dev/sda1 cat /payload.txt | grep -qx DADO-ORIGINAL-v1"
    "$BIN" delete vm "$DVM" -f >/dev/null 2>&1
  else
    "$BIN" delete vm "$DVM" -f >/dev/null 2>&1
  fi
  rm -f "$DDISK"
else
  skip "vm: restore recupera dados de disco" \
    "sem libvirt utilizável (virsh/qemu) ou sem guestfish"
fi

########################################
section "vm: o cloud-init do guest CONSOME o seed NoCloud (libvirt, guestfish)"
########################################
# Critério #16: ANEXAR um seed NoCloud e o `vm create` devolver 0 NÃO prova que
# o cloud-init do convidado o consumiu — o disco virtio `cidata` podia nunca ser
# lido (era exactamente por isto que `vm.cloud-init` do provider libvirt ficava
# `partial`: «the battery never logs into a guest»). Aqui boota-se um guest REAL
# (imagem cloud com cloud-init a bordo) a partir do caminho declarativo do
# engine — que GERA o seed NoCloud via `cloud-localds` e o anexa como disco
# virtio — com um `--user-data` que escreve um MARCADOR conhecido. Dá-se tempo
# ao cloud-init, PÁRA-SE a VM (o QEMU segura o qcow2 em exclusivo enquanto corre)
# e LÊ-SE o disco com guestfish:
#   1. o seed está anexado como disco virtio (`domblklist`),
#   2. o `write_files` escreveu o marcador — o cloud-init CORREU,
#   3. o hostname do seed foi aplicado — o `meta-data`/`user-data` foi lido,
#   4. o `cloud-init status` (escrito pelo `runcmd`, logo o runcmd correu) nomeia
#      a fonte: `DataSourceNoCloud [seed=/dev/vdb]` — o próprio guest a declarar
#      que identificou e leu o seed do disco virtio que o provider anexou.
# É a prova de que o seed CORREU dentro do guest, não só de que foi gerado.
#
# Precisa de uma imagem cloud com cloud-init a bordo (E2E_CLOUDINIT_IMAGE, ou uma
# descoberta em $SROOT/vm-images/_base). Sem ela, SKIP honesto — a bateria em
# CI não transporta uma imagem de arranque.
CIIMG="${E2E_CLOUDINIT_IMAGE:-}"
if [[ -z "$CIIMG" ]]; then
  CIIMG="$(ls -1 "$SROOT"/vm-images/_base/*cloudimg*.img 2>/dev/null | head -1 || true)"
fi
if libvirt_system_usable && command -v guestfish >/dev/null 2>&1 \
     && command -v cloud-localds >/dev/null 2>&1 && [[ -n "$CIIMG" && -r "$CIIMG" ]]; then
  CIVM="ciseed-$PFX"; CIBASE="$OUT/$CIVM-base.qcow2"; CIUD="$OUT/$CIVM-user-data.yaml"
  cp "$CIIMG" "$CIBASE" && qemu-img resize "$CIBASE" 5G >/dev/null 2>&1
  cat > "$CIUD" <<'YAML'
#cloud-config
hostname: delonix-ci-proof
package_update: false
package_upgrade: false
write_files:
  - path: /var/lib/delonix-cloudinit-OK
    permissions: '0644'
    content: |
      SEED-CONSUMED-BY-GUEST-CLOUDINIT-v1
runcmd:
  - [ sh, -c, "cloud-init status --long > /var/lib/delonix-cloudinit-STATUS 2>&1 || true" ]
YAML
  if check_then "vm create libvirt (cloud-init seed)" ok \
       "$BIN" vm create "$CIVM" --disk "$CIBASE" --backend libvirt --memory 1G --vcpus 2 --user-data "$CIUD"; then
    CIOVL="$SROOT/vms/$CIVM.qcow2"
    # O engine gerou o seed NoCloud e anexou-o como disco virtio read-only.
    check "o seed NoCloud esta anexado como disco virtio" ok bash -c \
      "virsh -c qemu:///system domblklist '$CIVM' | grep -q 'vms/$CIVM/seed.iso'"
    # Dar tempo ao cloud-init do guest (write_files + runcmd). A imagem vem
    # pronta (package_update:false), por isso ~2min bastam com folga.
    sleep 120
    # Parar a VM: só com o qcow2 livre é que o guestfish lê o disco de arranque.
    "$BIN" vm stop "$CIVM" >/dev/null 2>&1 || true
    for _ in $(seq 1 12); do
      virsh -c qemu:///system domstate "$CIVM" 2>/dev/null | grep -q 'shut off' && break
      [[ -z "$(virsh -c qemu:///system domstate "$CIVM" 2>/dev/null)" ]] && break
      sleep 2
    done
    # A PROVA: o cloud-init do guest escreveu o marcador do write_files.
    check "o cloud-init do guest escreveu o marcador do write_files" ok bash -c \
      "guestfish --ro -a '$CIOVL' -i cat /var/lib/delonix-cloudinit-OK | grep -qx SEED-CONSUMED-BY-GUEST-CLOUDINIT-v1"
    # O hostname do seed foi aplicado — o meta-data/user-data foi lido.
    check "o cloud-init aplicou o hostname do seed" ok bash -c \
      "guestfish --ro -a '$CIOVL' -i cat /etc/hostname | grep -qx delonix-ci-proof"
    # O runcmd correu E o guest nomeia a fonte: o seed NoCloud no disco virtio.
    check "o guest identificou e leu o seed NoCloud do disco virtio (runcmd correu)" ok bash -c \
      "guestfish --ro -a '$CIOVL' -i cat /var/lib/delonix-cloudinit-STATUS | grep -q 'DataSourceNoCloud'"
    "$BIN" delete vm "$CIVM" -f >/dev/null 2>&1
  else
    "$BIN" delete vm "$CIVM" -f >/dev/null 2>&1
  fi
  rm -f "$CIBASE" "$CIUD"
else
  skip "vm: o cloud-init do guest consome o seed NoCloud" \
    "sem libvirt utilizável, sem guestfish/cloud-localds, ou sem imagem cloud (E2E_CLOUDINIT_IMAGE)"
fi

########################################
section "vm: um disco a QUENTE e detectado e fica R/W DENTRO do guest (libvirt, cloud-init, guestfish)"
########################################
# §7 do mandato: `vm.hotplug.disk.add` do provider libvirt estava `partial`
# («attaches the same <disk>… never run against a guest»). Anexar um `<disk>` ao
# vivo e o `domblklist` passar a listá-lo prova que o HIPERVISOR aceitou (níveis
# 1-2), mas NÃO prova que o convidado vê o disco nem que ele serve para alguma
# coisa — era exactamente o buraco que mantinha a capacidade a meio. Aqui
# fecham-se os QUATRO níveis, ao vivo, contra um guest REAL:
#   1. PEDIDO ACEITE — `vm update --disk-add` passa pelo provider e o
#      `attach-device --live` devolve ok (o 124 `DX-8504` que a CLI imprime é só
#      o canal do guest-agent a dizer «não confirmo» — o disco É anexado, e os
#      níveis 3-4 abaixo provam-no por um caminho independente do agente);
#   2. APLICADO AO DISPOSITIVO VIRTUAL — o `domblklist` do domínio passa a listar
#      o disco quente num alvo novo (`vdc`);
#   3. DETECTADO PELO GUEST — dentro do convidado aparece um bloco novo em
#      `/sys/block` e no `lsblk` (um poller plantado por cloud-init regista-o);
#   4. UTILIZÁVEL — o guest ESCREVE um marcador conhecido no disco quente e
#      RELÊ-O igual (write+read reais ao bloco); o veredicto fica num ficheiro na
#      raiz, lido depois com guestfish (a VM parada, que o QEMU larga o qcow2).
# A detecção e o R/W NÃO passam pelo guest-agent: o poller corre dentro do guest
# e grava a prova no disco de arranque, por isso esta secção vale mesmo num guest
# sem agente — é o teste do RECURSO, não do canal de confirmação.
#
# Precisa de uma imagem cloud com cloud-init a bordo (E2E_CLOUDINIT_IMAGE, ou uma
# descoberta em $SROOT/vm-images/_base), guestfish e cloud-localds. Sem isso,
# SKIP honesto — a bateria em CI não transporta uma imagem de arranque.
HPIMG="${E2E_CLOUDINIT_IMAGE:-}"
if [[ -z "$HPIMG" ]]; then
  HPIMG="$(ls -1 "$SROOT"/vm-images/_base/*cloudimg*.img 2>/dev/null | head -1 || true)"
fi
if libvirt_system_usable && command -v guestfish >/dev/null 2>&1 \
     && command -v cloud-localds >/dev/null 2>&1 && [[ -n "$HPIMG" && -r "$HPIMG" ]]; then
  HPVM="hpadd-$PFX"; HPBASE="$OUT/$HPVM-base.qcow2"; HPHOT="$OUT/$HPVM-hot.qcow2"
  HPUD="$OUT/$HPVM-user-data.yaml"; HPPOLL="$OUT/$HPVM-poll.sh"
  cp "$HPIMG" "$HPBASE" && qemu-img resize "$HPBASE" 5G >/dev/null 2>&1
  qemu-img create -f qcow2 "$HPHOT" 128M >/dev/null 2>&1
  # O poller in-guest: espera por um disco que apareça DEPOIS do arranque (os de
  # boot/seed são a baseline), prova-o com um write+read ao bloco cru e grava o
  # veredicto na raiz, para o guestfish ler com a VM parada.
  cat > "$HPPOLL" <<'POLL'
#!/bin/bash
R=/var/lib/delonix-hotplug-RESULT; L=/var/lib/delonix-hotplug-LSBLK
exec >>/var/lib/delonix-hotplug-LOG 2>&1
blocks() { lsblk -dn -o NAME,TYPE | awk '$2=="disk"{print $1}' | sort; }
baseline="$(blocks)"
for _ in $(seq 1 600); do
  new="$(comm -13 <(echo "$baseline") <(blocks))"
  if [ -n "$new" ]; then
    dev="$(echo "$new" | head -1)"
    { echo "== lsblk =="; lsblk; echo "== /sys/block =="; ls -l /sys/block; } > "$L" 2>&1
    pat="HOTPLUG-RW-PROOF-v1"
    printf '%s\n' "$pat" | dd of="/dev/$dev" bs=512 seek=0 conv=notrunc 2>/dev/null
    blockdev --flushbufs "/dev/$dev" 2>/dev/null || true; sync
    rb="$(dd if=/dev/$dev bs=512 count=1 2>/dev/null | tr -d '\0' | head -c 32)"
    if [ "$rb" = "$pat" ]; then echo "DETECT=$dev WRITE=ok READ=ok MATCH=ok" > "$R"
    else echo "DETECT=$dev WRITE=sent READ=[$rb] MATCH=no" > "$R"; fi
    sync; break
  fi
  sleep 1
done
POLL
  {
    echo "#cloud-config"
    echo "hostname: dlx-hotplug-proof"
    echo "package_update: false"
    echo "package_upgrade: false"
    echo "write_files:"
    echo "  - path: /usr/local/bin/hotplug-poll.sh"
    echo "    permissions: '0755'"
    echo "    encoding: b64"
    echo "    content: $(base64 -w0 "$HPPOLL")"
    echo "runcmd:"
    echo "  - [ sh, -c, \"setsid bash /usr/local/bin/hotplug-poll.sh < /dev/null >/dev/null 2>&1 &\" ]"
  } > "$HPUD"
  if check_then "vm create libvirt (hotplug-disk guest)" ok \
       "$BIN" vm create "$HPVM" --disk "$HPBASE" --backend libvirt --memory 1G --vcpus 2 --user-data "$HPUD"; then
    HPOVL="$SROOT/vms/$HPVM.qcow2"
    # Dar tempo ao guest de arrancar e o cloud-init plantar o poller.
    sleep 100
    # NÍVEL 1: o provider aceita o pedido. A CLI sai 124 (`partial`) porque este
    # guest não tem agente para confirmar o nível 4 PELO canal do agente — mas o
    # attach-device ok imprime «attached», e os níveis 3-4 provam-no à parte.
    # Por isso aqui não se exige o rc 0; exige-se a MENSAGEM de attach do provider.
    check "nivel 1: o provider ACEITA o disco a quente (attach-device ok)" ok bash -c \
      "'$BIN' vm update '$HPVM' --disk-add '$HPHOT,bus=virtio,format=qcow2' --wait-secs 15 2>&1 | grep -q 'attached'"
    # NÍVEL 2: o disco quente está no domínio vivo, num alvo novo.
    check "nivel 2: o disco quente aparece no domblklist do dominio" ok bash -c \
      "virsh -c qemu:///system domblklist '$HPVM' | grep -q '$HPVM-hot.qcow2'"
    # Dar tempo ao poller in-guest de detectar + R/W.
    sleep 40
    # Parar a VM: só com o qcow2 livre é que o guestfish lê o disco de arranque.
    "$BIN" vm stop "$HPVM" >/dev/null 2>&1 || true
    for _ in $(seq 1 20); do
      virsh -c qemu:///system domstate "$HPVM" 2>/dev/null | grep -q 'shut off' && break
      [[ -z "$(virsh -c qemu:///system domstate "$HPVM" 2>/dev/null)" ]] && break
      sleep 2
    done
    # NÍVEL 3: o guest DETECTOU o bloco novo (o poller gravou lsblk + /sys/block
    # e nomeou o dispositivo quente).
    check "nivel 3: o guest DETECTA o disco quente (lsblk/sys-block)" ok bash -c \
      "guestfish --ro -a '$HPOVL' -i cat /var/lib/delonix-hotplug-LSBLK | grep -Eq 'vd[c-z] +[0-9].* disk'"
    # NÍVEL 4: escrita E leitura reais ao disco quente DENTRO do guest bateram.
    check "nivel 4: o disco quente e R/W DENTRO do guest (write+read batem)" ok bash -c \
      "guestfish --ro -a '$HPOVL' -i cat /var/lib/delonix-hotplug-RESULT | grep -q 'WRITE=ok READ=ok MATCH=ok'"
    "$BIN" delete vm "$HPVM" -f >/dev/null 2>&1
  else
    "$BIN" delete vm "$HPVM" -f >/dev/null 2>&1
  fi
  rm -f "$HPBASE" "$HPHOT" "$HPUD" "$HPPOLL"
else
  skip "vm: um disco a quente e detectado e fica R/W dentro do guest" \
    "sem libvirt utilizável, sem guestfish/cloud-localds, ou sem imagem cloud (E2E_CLOUDINIT_IMAGE)"
fi

########################################
section "vm: uma NIC a QUENTE e detectada e passa TRAFEGO REAL DENTRO do guest (libvirt, cloud-init, guestfish)"
########################################
# §7 do mandato: `vm.hotplug.nic.add` do provider libvirt estava `partial`
# («attaches the same <interface>… never run against a guest»). Anexar uma
# `<interface>` ao vivo e o `domiflist` passar a listá-la prova que o HIPERVISOR
# aceitou (níveis 1-2), mas NÃO prova que o convidado vê a NIC nem que ela serve
# para passar tráfego — era exactamente o buraco que mantinha a capacidade a
# meio (o mesmo buraco que a frente irmã do DISCO fechou). Aqui fecham-se os
# QUATRO níveis, ao vivo, contra um guest REAL:
#   1. PEDIDO ACEITE — `vm update --nic-add` passa pelo provider e o
#      `attach-device --live` devolve ok (o 124 `DX-8504` que a CLI imprime é só
#      o canal do guest-agent a dizer «não confirmo» — a NIC É anexada, e os
#      níveis 3-4 abaixo provam-no por um caminho independente do agente);
#   2. APLICADO AO DISPOSITIVO VIRTUAL — o `domiflist` do domínio passa a listar
#      a interface quente na rede nova (`ingress`);
#   3. DETECTADA PELO GUEST — dentro do convidado aparece uma NIC nova em
#      `/sys/class/net` e no `ip link` (um poller plantado por cloud-init
#      baseline-a as NICs de arranque e regista a que aparece DEPOIS);
#   4. TRAFEGO REAL — o guest CONFIGURA a NIC quente (IP estático na sub-rede da
#      rede `ingress`) e GERA tráfego para o gateway (`ping` a 192.168.199.1); o
#      veredicto grava o nº de respostas recebidas e os deltas rx/tx dos
#      contadores da interface, prova de que a NIC é UTILIZÁVEL — não só presente
#      — e fica num ficheiro na raiz, lido depois com guestfish (a VM parada, que
#      o QEMU larga o qcow2).
# A detecção e o tráfego NÃO passam pelo guest-agent: o poller corre dentro do
# guest e grava a prova no disco de arranque, por isso esta secção vale mesmo num
# guest sem agente — é o teste do RECURSO, não do canal de confirmação. O ping
# ao gateway da rede `ingress` (NAT, com gateway 192.168.199.1 na bridge do
# host) é um alvo alcançável rootless: o host responde ao ICMP na sua ponta da
# bridge, por isso o nível 4 é um ping end-to-end REAL, com fallback escrito para
# os contadores rx/tx caso o ICMP ao gateway esteja filtrado.
#
# Precisa de uma imagem cloud com cloud-init a bordo (E2E_CLOUDINIT_IMAGE, ou uma
# descoberta em $SROOT/vm-images/_base), guestfish, cloud-localds e a rede
# libvirt `ingress`. Sem isso, SKIP honesto — a bateria em CI não transporta uma
# imagem de arranque nem a rede NAT.
NICIMG="${E2E_CLOUDINIT_IMAGE:-}"
if [[ -z "$NICIMG" ]]; then
  NICIMG="$(ls -1 "$SROOT"/vm-images/_base/*cloudimg*.img 2>/dev/null | head -1 || true)"
fi
if libvirt_system_usable && command -v guestfish >/dev/null 2>&1 \
     && command -v cloud-localds >/dev/null 2>&1 && [[ -n "$NICIMG" && -r "$NICIMG" ]] \
     && virsh -c qemu:///system net-info ingress >/dev/null 2>&1; then
  NICVM="nicadd-$PFX"; NICBASE="$OUT/$NICVM-base.qcow2"
  NICUD="$OUT/$NICVM-user-data.yaml"; NICPOLL="$OUT/$NICVM-poll.sh"
  cp "$NICIMG" "$NICBASE" && qemu-img resize "$NICBASE" 5G >/dev/null 2>&1
  # O poller in-guest: baseline-a as NICs físicas de arranque, espera por uma que
  # apareça DEPOIS (a hot-added), configura-a com IP estático na sub-rede da rede
  # `ingress`, faz ping ao gateway e grava o veredicto (respostas + deltas rx/tx)
  # na raiz, para o guestfish ler com a VM parada.
  cat > "$NICPOLL" <<'POLL'
#!/bin/bash
R=/var/lib/delonix-nic-RESULT; L=/var/lib/delonix-nic-IPLINK
exec >>/var/lib/delonix-nic-LOG 2>&1
nics() { for d in /sys/class/net/*; do n=$(basename "$d"); [ "$n" = lo ] && continue; [ -e "$d/device" ] || continue; echo "$n"; done | sort; }
baseline="$(nics)"
for _ in $(seq 1 600); do
  new="$(comm -13 <(echo "$baseline") <(nics))"
  if [ -n "$new" ]; then
    dev="$(echo "$new" | head -1)"
    { echo "== ip link =="; ip link; echo "== all nics =="; nics; } > "$L" 2>&1
    ip link set "$dev" up
    ip addr add 192.168.199.200/24 dev "$dev"
    sleep 3
    rxb=$(cat "/sys/class/net/$dev/statistics/rx_packets")
    txb=$(cat "/sys/class/net/$dev/statistics/tx_packets")
    ping -c 4 -W 2 -I "$dev" 192.168.199.1 > /var/lib/delonix-nic-PING 2>&1
    rcv=$(grep -oE '[0-9]+ received' /var/lib/delonix-nic-PING | grep -oE '^[0-9]+')
    rxa=$(cat "/sys/class/net/$dev/statistics/rx_packets")
    txa=$(cat "/sys/class/net/$dev/statistics/tx_packets")
    rxd=$((rxa-rxb)); txd=$((txa-txb))
    if [ "${rcv:-0}" -ge 1 ]; then traf=ok-ping
    elif [ "$txd" -gt 0 ] && [ "$rxd" -gt 0 ]; then traf=ok-counters
    else traf=no; fi
    echo "DETECT=$dev PING_RECV=${rcv:-0} RX_DELTA=$rxd TX_DELTA=$txd TRAFFIC=$traf" > "$R"
    sync; break
  fi
  sleep 1
done
POLL
  {
    echo "#cloud-config"
    echo "hostname: dlx-nic-proof"
    echo "package_update: false"
    echo "package_upgrade: false"
    echo "write_files:"
    echo "  - path: /usr/local/bin/nic-poll.sh"
    echo "    permissions: '0755'"
    echo "    encoding: b64"
    echo "    content: $(base64 -w0 "$NICPOLL")"
    echo "runcmd:"
    echo "  - [ sh, -c, \"setsid bash /usr/local/bin/nic-poll.sh < /dev/null >/dev/null 2>&1 &\" ]"
  } > "$NICUD"
  if check_then "vm create libvirt (hotplug-nic guest)" ok \
       "$BIN" vm create "$NICVM" --disk "$NICBASE" --backend libvirt --memory 1G --vcpus 2 --user-data "$NICUD"; then
    NICOVL="$SROOT/vms/$NICVM.qcow2"
    # Dar tempo ao guest de arrancar e o cloud-init plantar o poller.
    sleep 100
    # NÍVEL 1: o provider aceita o pedido. A CLI sai 124 (`partial`) porque este
    # guest não tem agente para confirmar o nível 4 PELO canal do agente — mas o
    # attach-device ok imprime «NIC on ingress attached», e os níveis 3-4
    # provam-no à parte. Por isso aqui não se exige o rc 0; exige-se a MENSAGEM
    # de attach do provider.
    check "nivel 1: o provider ACEITA a NIC a quente (attach-device ok)" ok bash -c \
      "'$BIN' vm update '$NICVM' --nic-add 'network:ingress' --wait-secs 15 2>&1 | grep -q 'NIC on ingress attached'"
    # NÍVEL 2: a interface quente está no domínio vivo, na rede nova.
    check "nivel 2: a NIC quente aparece no domiflist do dominio (rede ingress)" ok bash -c \
      "virsh -c qemu:///system domiflist '$NICVM' | grep -qw ingress"
    # Dar tempo ao poller in-guest de detectar + configurar + gerar tráfego.
    sleep 40
    # Parar a VM: só com o qcow2 livre é que o guestfish lê o disco de arranque.
    "$BIN" vm stop "$NICVM" >/dev/null 2>&1 || true
    for _ in $(seq 1 20); do
      virsh -c qemu:///system domstate "$NICVM" 2>/dev/null | grep -q 'shut off' && break
      [[ -z "$(virsh -c qemu:///system domstate "$NICVM" 2>/dev/null)" ]] && break
      sleep 2
    done
    # NÍVEL 3: o guest DETECTOU a NIC nova (o poller gravou ip link + a lista de
    # NICs e nomeou a que apareceu depois do baseline de arranque).
    check "nivel 3: o guest DETECTA a NIC quente (ip link/sys-class-net)" ok bash -c \
      "guestfish --ro -a '$NICOVL' -i cat /var/lib/delonix-nic-RESULT | grep -Eq 'DETECT=[a-z0-9]+ '"
    # NÍVEL 4: a NIC quente PASSOU TRÁFEGO REAL de dentro do guest — ping ao
    # gateway recebido (end-to-end) OU contadores rx/tx a subir (fallback).
    check "nivel 4: a NIC quente PASSA TRAFEGO REAL DENTRO do guest (ping/contadores)" ok bash -c \
      "guestfish --ro -a '$NICOVL' -i cat /var/lib/delonix-nic-RESULT | grep -Eq 'TRAFFIC=ok-(ping|counters)'"
    "$BIN" delete vm "$NICVM" -f >/dev/null 2>&1
  else
    "$BIN" delete vm "$NICVM" -f >/dev/null 2>&1
  fi
  rm -f "$NICBASE" "$NICUD" "$NICPOLL"
else
  skip "vm: uma NIC a quente e detectada e passa trafego real dentro do guest" \
    "sem libvirt utilizável, sem guestfish/cloud-localds, sem imagem cloud (E2E_CLOUDINIT_IMAGE), ou sem a rede libvirt ingress"
fi

########################################
section "vm: um disco a QUENTE REMOVIDO com seguranca DENTRO do guest (libvirt, cloud-init, guestfish)"
########################################
# §7 do mandato: `vm.hotplug.disk.remove` do provider libvirt estava
# `not-implemented` (D10 — hot-remove não se assume do hot-add). A remoção tem
# de ser SEGURA: o `detach-device --live` é COOPERATIVO — a libvirt pede ao
# guest que largue o dispositivo e o domínio só o solta quando o guest o faz;
# um disco que o guest ainda segura (montado, em uso) NUNCA sai, e o provider
# devolve `partial` com o registo intacto (nada é arrancado de baixo do guest).
# Aqui provam-se os QUATRO níveis, ao vivo, contra um guest REAL, e a SEGURANÇA:
#   0. PREPARAÇÃO SEGURA — o disco extra é anexado a quente, o poller formata-o,
#      MONTA-o, escreve nele e DESMONTA-o (larga-o) ANTES de qualquer remoção;
#   1. PEDIDO ACEITE — `vm update --disk-rm` passa pelo provider e o
#      `detach-device --live` devolve ok (a CLI imprime «removed»; o 124
#      `DX-8504` é só o canal do guest-agent a não confirmar — os níveis 3-4
#      provam-no por um caminho independente do agente);
#   2. SAIU DO DISPOSITIVO VIRTUAL — o `domblklist` do domínio DEIXA de o listar
#      (o guest largou-o; sem guest-agent, é o kernel do guest a cooperar);
#   3. O GUEST DEIXA DE O VER — o bloco some de `/sys/block` dentro do guest (o
#      poller regista a sua ausência);
#   4. O GUEST CONTINUA SÃO — não entrou em pânico, o disco de arranque (raiz)
#      continua R/W (write+read reais à raiz depois da remoção); o veredicto fica
#      na raiz, lido com guestfish (VM parada, QEMU larga o qcow2).
#
# Precisa de uma imagem cloud (E2E_CLOUDINIT_IMAGE ou $SROOT/vm-images/_base),
# guestfish e cloud-localds. Sem isso, SKIP honesto.
HRIMG="${E2E_CLOUDINIT_IMAGE:-}"
if [[ -z "$HRIMG" ]]; then
  HRIMG="$(ls -1 "$SROOT"/vm-images/_base/*cloudimg*.img 2>/dev/null | head -1 || true)"
fi
if libvirt_system_usable && command -v guestfish >/dev/null 2>&1 \
     && command -v cloud-localds >/dev/null 2>&1 && [[ -n "$HRIMG" && -r "$HRIMG" ]]; then
  HRVM="hprm-$PFX"; HRBASE="$OUT/$HRVM-base.qcow2"; HRHOT="$OUT/$HRVM-hot.qcow2"
  HRUD="$OUT/$HRVM-user-data.yaml"; HRPOLL="$OUT/$HRVM-poll.sh"
  cp "$HRIMG" "$HRBASE" && qemu-img resize "$HRBASE" 5G >/dev/null 2>&1
  qemu-img create -f qcow2 "$HRHOT" 128M >/dev/null 2>&1
  # O poller in-guest: FASE A espera o disco quente aparecer, formata-o, monta-o,
  # escreve, DESMONTA-o (larga-o em segurança) e regista PREP. FASE B espera-o
  # DESAPARECER (a remoção), confirma que some de /sys/block e que a RAIZ continua
  # R/W (guest são), e grava RESULT na raiz.
  cat > "$HRPOLL" <<'POLL'
#!/bin/bash
P=/var/lib/delonix-diskrm-PREP; R=/var/lib/delonix-diskrm-RESULT
exec >>/var/lib/delonix-diskrm-LOG 2>&1
blocks() { lsblk -dn -o NAME,TYPE | awk '$2=="disk"{print $1}' | sort; }
baseline="$(blocks)"
dev=""
for _ in $(seq 1 600); do
  new="$(comm -13 <(echo "$baseline") <(blocks))"
  if [ -n "$new" ]; then
    dev="$(echo "$new" | head -1)"
    mkfs.ext4 -F "/dev/$dev" >/dev/null 2>&1
    mkdir -p /mnt/hot && mount "/dev/$dev" /mnt/hot
    echo DISK-IN-USE-v1 > /mnt/hot/marker; sync
    umount /mnt/hot
    echo "HOTDEV=$dev MOUNTED=ok UNMOUNTED=ok" > "$P"; sync
    break
  fi
  sleep 1
done
[ -z "$dev" ] && { echo "HOTDEV=none" > "$P"; sync; exit 0; }
for _ in $(seq 1 600); do
  if ! blocks | grep -qx "$dev"; then
    gone=0; [ -e "/sys/block/$dev" ] && gone=1
    echo alive-$(date +%s) > /root/diskrm-alive; sync
    rb="$(cat /root/diskrm-alive 2>/dev/null)"
    [ -n "$rb" ] && rw=ok || rw=no
    echo "REMOVED=$dev SYS_PRESENT=$gone ROOT_RW=$rw GUEST=alive" > "$R"; sync
    break
  fi
  sleep 1
done
POLL
  {
    echo "#cloud-config"
    echo "hostname: dlx-diskrm-proof"
    echo "package_update: false"
    echo "package_upgrade: false"
    echo "write_files:"
    echo "  - path: /usr/local/bin/diskrm-poll.sh"
    echo "    permissions: '0755'"
    echo "    encoding: b64"
    echo "    content: $(base64 -w0 "$HRPOLL")"
    echo "runcmd:"
    echo "  - [ sh, -c, \"setsid bash /usr/local/bin/diskrm-poll.sh < /dev/null >/dev/null 2>&1 &\" ]"
  } > "$HRUD"
  if check_then "vm create libvirt (hotplug-disk-remove guest)" ok \
       "$BIN" vm create "$HRVM" --disk "$HRBASE" --backend libvirt --memory 1G --vcpus 2 --user-data "$HRUD"; then
    HROVL="$SROOT/vms/$HRVM.qcow2"
    # Dar tempo ao guest de arrancar e o cloud-init plantar o poller.
    sleep 100
    # ADD do disco extra (caminho já provado) — o ALVO vivo que o provider dá.
    "$BIN" vm update "$HRVM" --disk-add "$HRHOT,bus=virtio,format=qcow2" --wait-secs 15 >/dev/null 2>&1 || true
    # NÍVEL 0 (preparação segura): dar tempo ao poller de formatar/montar/escrever
    # e DESMONTAR o disco quente — larga-o ANTES de qualquer remoção. SEM parar a
    # VM aqui: um stop mataria o poller (o cloud-init runcmd não re-corre), e a
    # FASE B dele (detectar a remoção) nunca aconteceria.
    sleep 45
    # NÍVEL 1: o provider ACEITA a remoção (detach-device ok; CLI imprime «removed»;
    # o 124/`partial` é só o guest-agent). A remoção é COOPERATIVA: o disco já foi
    # desmontado, por isso o guest larga-o.
    check "nivel 1: o provider ACEITA a remocao a quente (detach-device ok)" ok bash -c \
      "'$BIN' vm update '$HRVM' --disk-rm '$HRHOT' --wait-secs 30 2>&1 | grep -q 'removed'"
    # NÍVEL 2: o disco quente SAIU do domínio (o guest cooperou e largou-o).
    check "nivel 2: o disco quente SAI do domblklist do dominio" ok bash -c \
      "! virsh -c qemu:///system domblklist '$HRVM' | grep -q '$HRVM-hot.qcow2'"
    # Dar tempo ao poller (FASE B) de ver o bloco desaparecer e gravar RESULT.
    sleep 25
    # Parar a VM: só com o qcow2 livre é que o guestfish lê o disco de arranque.
    "$BIN" vm stop "$HRVM" >/dev/null 2>&1 || true
    for _ in $(seq 1 20); do
      virsh -c qemu:///system domstate "$HRVM" 2>/dev/null | grep -q 'shut off' && break
      [[ -z "$(virsh -c qemu:///system domstate "$HRVM" 2>/dev/null)" ]] && break
      sleep 2
    done
    # NÍVEL 0 (lido no fim, do mesmo arranque): montado e DESMONTADO em segurança.
    check "nivel 0: o disco quente foi MONTADO e DESMONTADO (uso + libertacao segura)" ok bash -c \
      "guestfish --ro -a '$HROVL' -i cat /var/lib/delonix-diskrm-PREP | grep -q 'MOUNTED=ok UNMOUNTED=ok'"
    # NÍVEL 3: o guest DEIXOU de ver o bloco (some de /sys/block).
    check "nivel 3: o guest DEIXA de ver o disco quente (ausente de /sys/block)" ok bash -c \
      "guestfish --ro -a '$HROVL' -i cat /var/lib/delonix-diskrm-RESULT | grep -q 'SYS_PRESENT=0'"
    # NÍVEL 4: o guest continua SÃO — raiz R/W depois da remoção, sem pânico.
    check "nivel 4: o guest continua SAO (raiz R/W apos a remocao)" ok bash -c \
      "guestfish --ro -a '$HROVL' -i cat /var/lib/delonix-diskrm-RESULT | grep -q 'ROOT_RW=ok GUEST=alive'"
    "$BIN" delete vm "$HRVM" -f >/dev/null 2>&1
  else
    "$BIN" delete vm "$HRVM" -f >/dev/null 2>&1
  fi
  rm -f "$HRBASE" "$HRHOT" "$HRUD" "$HRPOLL"
else
  skip "vm: um disco a quente removido com seguranca dentro do guest" \
    "sem libvirt utilizável, sem guestfish/cloud-localds, ou sem imagem cloud (E2E_CLOUDINIT_IMAGE)"
fi

########################################
section "vm: uma NIC a QUENTE REMOVIDA DENTRO do guest sem derrubar a primaria (libvirt, cloud-init, guestfish)"
########################################
# §7 do mandato: `vm.hotplug.nic.remove` do provider libvirt estava
# `not-implemented` (D10). O `detach-device --live` matcha a interface pelo seu
# `<mac address>` e é cooperativo; a SEGURANÇA aqui é NÃO derrubar a NIC
# primária (o motor só remove NICs EXTRA, por MAC — a primária nunca está nos
# extra_nics) e provar que o guest continua com rede depois. Quatro níveis:
#   1. PEDIDO ACEITE — `vm update --nic-rm <mac>` e o `detach-device --live` ok
#      (CLI imprime «removed»; 124 `DX-8504` é só o guest-agent);
#   2. SAIU DO DISPOSITIVO VIRTUAL — o `domiflist` DEIXA de listar aquele MAC;
#   3. O GUEST DEIXA DE A VER — a NIC some de `/sys/class/net` dentro do guest;
#   4. O GUEST CONTINUA SÃO — a NIC PRIMÁRIA continua UP e com portadora (o guest
#      não perdeu a rede); veredicto na raiz, lido com guestfish (VM parada).
#
# Precisa de imagem cloud, guestfish, cloud-localds e a rede libvirt `ingress`.
NRIMG="${E2E_CLOUDINIT_IMAGE:-}"
if [[ -z "$NRIMG" ]]; then
  NRIMG="$(ls -1 "$SROOT"/vm-images/_base/*cloudimg*.img 2>/dev/null | head -1 || true)"
fi
if libvirt_system_usable && command -v guestfish >/dev/null 2>&1 \
     && command -v cloud-localds >/dev/null 2>&1 && [[ -n "$NRIMG" && -r "$NRIMG" ]] \
     && virsh -c qemu:///system net-info ingress >/dev/null 2>&1; then
  NRVM="nicrm-$PFX"; NRBASE="$OUT/$NRVM-base.qcow2"
  NRUD="$OUT/$NRVM-user-data.yaml"; NRPOLL="$OUT/$NRVM-poll.sh"
  cp "$NRIMG" "$NRBASE" && qemu-img resize "$NRBASE" 5G >/dev/null 2>&1
  # O poller: baseline das NICs de arranque (uma delas é a PRIMÁRIA), espera a
  # hot-added aparecer (FASE A, regista PREP), depois espera-a DESAPARECER
  # (FASE B) e confirma que a PRIMÁRIA continua UP com portadora (guest com rede).
  cat > "$NRPOLL" <<'POLL'
#!/bin/bash
P=/var/lib/delonix-nicrm-PREP; R=/var/lib/delonix-nicrm-RESULT
exec >>/var/lib/delonix-nicrm-LOG 2>&1
nics() { for d in /sys/class/net/*; do n=$(basename "$d"); [ "$n" = lo ] && continue; [ -e "$d/device" ] || continue; echo "$n"; done | sort; }
primary="$(nics | head -1)"
baseline="$(nics)"
dev=""
for _ in $(seq 1 600); do
  new="$(comm -13 <(echo "$baseline") <(nics))"
  if [ -n "$new" ]; then
    dev="$(echo "$new" | head -1)"
    echo "HOTNIC=$dev PRIMARY=$primary" > "$P"; sync
    break
  fi
  sleep 1
done
[ -z "$dev" ] && { echo "HOTNIC=none" > "$P"; sync; exit 0; }
for _ in $(seq 1 600); do
  if ! nics | grep -qx "$dev"; then
    gone=0; [ -e "/sys/class/net/$dev" ] && gone=1
    op="$(cat /sys/class/net/$primary/operstate 2>/dev/null)"
    car="$(cat /sys/class/net/$primary/carrier 2>/dev/null)"
    [ "$car" = 1 ] && prim=up || prim="$op"
    echo "REMOVED=$dev SYS_PRESENT=$gone PRIMARY=$primary PRIMARY_STATE=$prim GUEST=alive" > "$R"; sync
    break
  fi
  sleep 1
done
POLL
  {
    echo "#cloud-config"
    echo "hostname: dlx-nicrm-proof"
    echo "package_update: false"
    echo "package_upgrade: false"
    echo "write_files:"
    echo "  - path: /usr/local/bin/nicrm-poll.sh"
    echo "    permissions: '0755'"
    echo "    encoding: b64"
    echo "    content: $(base64 -w0 "$NRPOLL")"
    echo "runcmd:"
    echo "  - [ sh, -c, \"setsid bash /usr/local/bin/nicrm-poll.sh < /dev/null >/dev/null 2>&1 &\" ]"
  } > "$NRUD"
  if check_then "vm create libvirt (hotplug-nic-remove guest)" ok \
       "$BIN" vm create "$NRVM" --disk "$NRBASE" --backend libvirt --memory 1G --vcpus 2 --user-data "$NRUD"; then
    NROVL="$SROOT/vms/$NRVM.qcow2"
    sleep 100
    # ADD da NIC extra (caminho já provado) na rede `ingress`.
    "$BIN" vm update "$NRVM" --nic-add 'network:ingress' --wait-secs 15 >/dev/null 2>&1 || true
    sleep 15
    # O MAC da NIC quente — o que está na rede `ingress` no domiflist.
    NRMAC="$(virsh -c qemu:///system domiflist "$NRVM" 2>/dev/null | awk '$3=="ingress"{print $NF}' | head -1)"
    # NÍVEL 1: o provider ACEITA a remoção (detach-device ok; CLI imprime «removed»).
    check "nivel 1: o provider ACEITA a remocao da NIC a quente (detach-device ok)" ok bash -c \
      "[ -n '$NRMAC' ] && '$BIN' vm update '$NRVM' --nic-rm '$NRMAC' --wait-secs 30 2>&1 | grep -q 'removed'"
    # NÍVEL 2: a NIC quente SAIU do domínio (o MAC deixa o domiflist).
    check "nivel 2: a NIC quente SAI do domiflist do dominio" ok bash -c \
      "[ -n '$NRMAC' ] && ! virsh -c qemu:///system domiflist '$NRVM' | grep -qiw '$NRMAC'"
    sleep 20
    "$BIN" vm stop "$NRVM" >/dev/null 2>&1 || true
    for _ in $(seq 1 20); do
      virsh -c qemu:///system domstate "$NRVM" 2>/dev/null | grep -q 'shut off' && break
      [[ -z "$(virsh -c qemu:///system domstate "$NRVM" 2>/dev/null)" ]] && break
      sleep 2
    done
    # NÍVEL 3: o guest DEIXOU de ver a NIC (some de /sys/class/net).
    check "nivel 3: o guest DEIXA de ver a NIC quente (ausente de /sys/class/net)" ok bash -c \
      "guestfish --ro -a '$NROVL' -i cat /var/lib/delonix-nicrm-RESULT | grep -q 'SYS_PRESENT=0'"
    # NÍVEL 4: o guest continua SÃO — a NIC PRIMÁRIA continua UP (rede intacta).
    check "nivel 4: o guest continua SAO (NIC primaria UP apos a remocao)" ok bash -c \
      "guestfish --ro -a '$NROVL' -i cat /var/lib/delonix-nicrm-RESULT | grep -Eq 'PRIMARY_STATE=up .*GUEST=alive'"
    "$BIN" delete vm "$NRVM" -f >/dev/null 2>&1
  else
    "$BIN" delete vm "$NRVM" -f >/dev/null 2>&1
  fi
  rm -f "$NRBASE" "$NRUD" "$NRPOLL"
else
  skip "vm: uma NIC a quente removida dentro do guest sem derrubar a primaria" \
    "sem libvirt utilizável, sem guestfish/cloud-localds, sem imagem cloud (E2E_CLOUDINIT_IMAGE), ou sem a rede libvirt ingress"
fi

########################################
section "vm: um vCPU a QUENTE e ONLINE DENTRO do guest (libvirt, cloud-init, guestfish)"
########################################
# §7 do mandato: `vm.hotplug.cpu.add` do provider libvirt estava `partial`
# («`virsh setvcpus --live`… unit-tested, never run against a guest»). Mandar
# o `setvcpus --live` e o `vcpucount` passar a contar o vCPU novo prova que o
# HIPERVISOR aceitou (níveis 1-2), mas NÃO prova que o convidado vê o CPU nem
# que ele passa a contar para o trabalho — era o buraco que mantinha a
# capacidade a meio (o mesmo buraco que as frentes irmãs do DISCO e da NIC
# fecharam). Aqui fecham-se os QUATRO níveis, ao vivo, contra um guest REAL:
#   1. PEDIDO ACEITE — `vm update --vcpus 3` passa pelo provider e o
#      `setvcpus --live` devolve ok; a CLI imprime `vcpus -> 3` e sai 124
#      (`DX-8504`) porque sem guest-agent não há quem confirme o nível 4 PELO
#      canal do agente — mas o vCPU É anexado, e os níveis 3-4 abaixo provam-no
#      por um caminho independente do agente;
#   2. APLICADO AO DOMÍNIO — o `vcpucount --live --active` do domínio passa a
#      devolver 3 (o vCPU quente está no domínio vivo);
#   3. DETECTADO PELO GUEST — dentro do convidado aparece um `cpuN` novo em
#      `/sys/devices/system/cpu` (um poller plantado por cloud-init baseline-a
#      os cores de arranque e regista o directório que aparece DEPOIS);
#   4. ONLINE E A CONTAR — o guest põe o core quente online
#      (`echo 1 > /sys/devices/system/cpu/cpuN/online`, caso não venha sozinho)
#      e o `nproc` sobe acima do baseline de arranque — prova de que o vCPU é
#      UTILIZÁVEL, não só presente; o veredicto fica num ficheiro na raiz, lido
#      depois com guestfish (a VM parada, que o QEMU larga o qcow2).
# A detecção e o online NÃO passam pelo guest-agent: o poller corre dentro do
# guest e grava a prova no disco de arranque, por isso esta secção vale mesmo
# num guest sem agente — é o teste do RECURSO, não do canal de confirmação. A
# folga de hotplug vem do `--vcpus-max 4` no create (ADR-0068 D3): sem ela o
# `vm update` recusa por não haver teto acima do arranque.
#
# Precisa de uma imagem cloud com cloud-init a bordo (E2E_CLOUDINIT_IMAGE, ou
# uma descoberta em $SROOT/vm-images/_base), guestfish e cloud-localds. Sem
# isso, SKIP honesto — a bateria em CI não transporta uma imagem de arranque.
CPUIMG="${E2E_CLOUDINIT_IMAGE:-}"
if [[ -z "$CPUIMG" ]]; then
  CPUIMG="$(ls -1 "$SROOT"/vm-images/_base/*cloudimg*.img 2>/dev/null | head -1 || true)"
fi
if libvirt_system_usable && command -v guestfish >/dev/null 2>&1 \
     && command -v cloud-localds >/dev/null 2>&1 && [[ -n "$CPUIMG" && -r "$CPUIMG" ]]; then
  CPUVM="cpuadd-$PFX"; CPUBASE="$OUT/$CPUVM-base.qcow2"
  CPUUD="$OUT/$CPUVM-user-data.yaml"; CPUPOLL="$OUT/$CPUVM-poll.sh"
  cp "$CPUIMG" "$CPUBASE" && qemu-img resize "$CPUBASE" 5G >/dev/null 2>&1
  # O poller in-guest: baseline-a os cores de arranque (nproc + nº de dirs
  # cpuN), espera por um cpuN que apareça DEPOIS, põe-no online se vier offline,
  # e grava o veredicto (detecção + nproc antes/depois) na raiz para o guestfish
  # ler com a VM parada.
  cat > "$CPUPOLL" <<'POLL'
#!/bin/bash
R=/var/lib/delonix-cpu-RESULT
exec >>/var/lib/delonix-cpu-LOG 2>&1
base_nproc=$(nproc)
base_dirs=$(ls -d /sys/devices/system/cpu/cpu[0-9]* 2>/dev/null | wc -l)
for _ in $(seq 1 600); do
  for o in /sys/devices/system/cpu/cpu[0-9]*/online; do
    [ -f "$o" ] && [ "$(cat "$o")" = 0 ] && echo 1 > "$o" 2>/dev/null
  done
  now_nproc=$(nproc)
  now_dirs=$(ls -d /sys/devices/system/cpu/cpu[0-9]* 2>/dev/null | wc -l)
  if [ "$now_dirs" -gt "$base_dirs" ]; then det=ok; else det=no; fi
  if [ "$now_nproc" -gt "$base_nproc" ]; then up=ok; else up=no; fi
  if [ "$det" = ok ] && [ "$up" = ok ]; then
    { echo "== lscpu =="; lscpu 2>/dev/null; echo "== online =="; cat /sys/devices/system/cpu/online; } > /var/lib/delonix-cpu-INFO 2>&1
    echo "BASE_NPROC=$base_nproc NEW_NPROC=$now_nproc BASE_CPUDIRS=$base_dirs NEW_CPUDIRS=$now_dirs CPU_DETECT=$det NPROC_UP=$up" > "$R"
    sync; break
  fi
  sleep 1
done
POLL
  {
    echo "#cloud-config"
    echo "hostname: dlx-cpu-proof"
    echo "package_update: false"
    echo "package_upgrade: false"
    echo "write_files:"
    echo "  - path: /usr/local/bin/cpu-poll.sh"
    echo "    permissions: '0755'"
    echo "    encoding: b64"
    echo "    content: $(base64 -w0 "$CPUPOLL")"
    echo "runcmd:"
    echo "  - [ sh, -c, \"setsid bash /usr/local/bin/cpu-poll.sh < /dev/null >/dev/null 2>&1 &\" ]"
  } > "$CPUUD"
  if check_then "vm create libvirt (hotplug-cpu guest, headroom --vcpus-max 4)" ok \
       "$BIN" vm create "$CPUVM" --disk "$CPUBASE" --backend libvirt --memory 1G --vcpus 2 --vcpus-max 4 --user-data "$CPUUD"; then
    CPUOVL="$SROOT/vms/$CPUVM.qcow2"
    # Dar tempo ao guest de arrancar e o cloud-init plantar o poller.
    sleep 100
    # NÍVEL 1: o provider aceita o pedido. A CLI sai 124 (`partial`) por falta de
    # agente; exige-se a MENSAGEM `vcpus -> 3` do provider, não o rc 0.
    check "nivel 1: o provider ACEITA o vCPU a quente (setvcpus live)" ok bash -c \
      "'$BIN' vm update '$CPUVM' --vcpus 3 --wait-secs 15 2>&1 | grep -Eq 'vcpus -> 3 \\((partial|complete)\\)'"
    # NÍVEL 2: o vCPU quente está no domínio vivo (vcpucount activo = 3).
    check "nivel 2: o vCPU quente conta no domínio (vcpucount live active = 3)" ok bash -c \
      "[ \"\$(virsh -c qemu:///system vcpucount '$CPUVM' --live --active 2>/dev/null | tr -d '[:space:]')\" = 3 ]"
    # Dar tempo ao poller in-guest de detectar + pôr online.
    sleep 40
    # Parar a VM: só com o qcow2 livre é que o guestfish lê o disco de arranque.
    "$BIN" vm stop "$CPUVM" >/dev/null 2>&1 || true
    for _ in $(seq 1 20); do
      virsh -c qemu:///system domstate "$CPUVM" 2>/dev/null | grep -q 'shut off' && break
      [[ -z "$(virsh -c qemu:///system domstate "$CPUVM" 2>/dev/null)" ]] && break
      sleep 2
    done
    # NÍVEL 3: o guest DETECTOU o core novo (apareceu um cpuN acima do baseline).
    check "nivel 3: o guest DETECTA o vCPU quente (novo cpuN em sys-devices)" ok bash -c \
      "guestfish --ro -a '$CPUOVL' -i cat /var/lib/delonix-cpu-RESULT | grep -q 'CPU_DETECT=ok'"
    # NÍVEL 4: o core quente está ONLINE e a contar — nproc subiu acima do boot.
    check "nivel 4: o vCPU quente fica ONLINE DENTRO do guest (nproc sobe)" ok bash -c \
      "guestfish --ro -a '$CPUOVL' -i cat /var/lib/delonix-cpu-RESULT | grep -q 'NPROC_UP=ok'"
    "$BIN" delete vm "$CPUVM" -f >/dev/null 2>&1
  else
    "$BIN" delete vm "$CPUVM" -f >/dev/null 2>&1
  fi
  rm -f "$CPUBASE" "$CPUUD" "$CPUPOLL"
else
  skip "vm: um vCPU a quente e online dentro do guest" \
    "sem libvirt utilizável, sem guestfish/cloud-localds, ou sem imagem cloud (E2E_CLOUDINIT_IMAGE)"
fi

########################################
section "vm: memoria a QUENTE e ONLINE DENTRO do guest (libvirt, cloud-init, guestfish)"
########################################
# §7 do mandato: `vm.hotplug.memory.add` do provider libvirt estava `partial`
# («plugs a DIMM (`attach-device --live`)… unit-tested, never run against a
# guest»). Anexar um `<memory model='dimm'>` ao vivo e o `dumpxml`/`dominfo`
# passar a mostrá-lo prova que o HIPERVISOR aceitou (níveis 1-2), mas NÃO prova
# que o convidado vê a RAM nova nem que ela entra no `MemTotal` — era o buraco
# que mantinha a capacidade a meio. Aqui fecham-se os QUATRO níveis, ao vivo,
# contra um guest REAL:
#   1. PEDIDO ACEITE — `vm update --memory 2G` passa pelo provider e o
#      `attach-device --live` devolve ok; a CLI imprime `memory -> 2048M` e sai
#      124 (`DX-8504`) porque sem guest-agent não há quem confirme o nível 4
#      PELO canal do agente — mas o DIMM É anexado, e os níveis 3-4 provam-no à
#      parte;
#   2. APLICADO AO DOMÍNIO — o `dumpxml` do domínio vivo passa a listar o
#      `<memory model='dimm'>` quente (a RAM nova está no domínio);
#   3. DETECTADA PELO GUEST — dentro do convidado aparecem blocos novos em
#      `/sys/devices/system/memory` (um poller plantado por cloud-init baseline-a
#      os blocos de arranque e regista os que aparecem DEPOIS);
#   4. ONLINE E A CONTAR — o guest põe os blocos quentes online
#      (`echo online > /sys/devices/system/memory/memoryN/state`, caso não venham
#      sozinhos) e o `MemTotal` do `/proc/meminfo` sobe acima do baseline de
#      arranque — prova de que a RAM é UTILIZÁVEL, não só presente; o veredicto
#      fica num ficheiro na raiz, lido depois com guestfish (a VM parada).
# A detecção e o online NÃO passam pelo guest-agent: o poller corre dentro do
# guest e grava a prova no disco de arranque, por isso esta secção vale mesmo
# num guest sem agente — é o teste do RECURSO, não do canal de confirmação. A
# folga de hotplug vem do `--memory-max 2G` no create (ADR-0068 D3): reserva os
# slots `<maxMemory>` e a topologia de nó NUMA que um DIMM precisa.
#
# Precisa de uma imagem cloud com cloud-init a bordo (E2E_CLOUDINIT_IMAGE, ou
# uma descoberta em $SROOT/vm-images/_base), guestfish e cloud-localds. Sem
# isso, SKIP honesto — a bateria em CI não transporta uma imagem de arranque.
MEMIMG="${E2E_CLOUDINIT_IMAGE:-}"
if [[ -z "$MEMIMG" ]]; then
  MEMIMG="$(ls -1 "$SROOT"/vm-images/_base/*cloudimg*.img 2>/dev/null | head -1 || true)"
fi
if libvirt_system_usable && command -v guestfish >/dev/null 2>&1 \
     && command -v cloud-localds >/dev/null 2>&1 && [[ -n "$MEMIMG" && -r "$MEMIMG" ]]; then
  MEMVM="memadd-$PFX"; MEMBASE="$OUT/$MEMVM-base.qcow2"
  MEMUD="$OUT/$MEMVM-user-data.yaml"; MEMPOLL="$OUT/$MEMVM-poll.sh"
  cp "$MEMIMG" "$MEMBASE" && qemu-img resize "$MEMBASE" 5G >/dev/null 2>&1
  # O poller in-guest: baseline-a o MemTotal e os blocos de memória de arranque,
  # espera por blocos que apareçam DEPOIS, põe-nos online se vierem offline, e
  # grava o veredicto (MemTotal antes/depois) na raiz para o guestfish ler com a
  # VM parada.
  cat > "$MEMPOLL" <<'POLL'
#!/bin/bash
R=/var/lib/delonix-mem-RESULT
exec >>/var/lib/delonix-mem-LOG 2>&1
memtotal() { awk '/^MemTotal:/{print $2}' /proc/meminfo; }
base_mt=$(memtotal)
base_blk=$(ls -d /sys/devices/system/memory/memory[0-9]* 2>/dev/null | wc -l)
for _ in $(seq 1 600); do
  for s in /sys/devices/system/memory/memory[0-9]*/state; do
    [ -f "$s" ] && [ "$(cat "$s")" = offline ] && echo online > "$s" 2>/dev/null
  done
  now_mt=$(memtotal)
  now_blk=$(ls -d /sys/devices/system/memory/memory[0-9]* 2>/dev/null | wc -l)
  if [ "$now_blk" -gt "$base_blk" ]; then det=ok; else det=no; fi
  if [ "$now_mt" -gt "$base_mt" ]; then up=ok; else up=no; fi
  if [ "$det" = ok ] && [ "$up" = ok ]; then
    echo "BASE_MEMTOTAL_KB=$base_mt NEW_MEMTOTAL_KB=$now_mt BASE_BLOCKS=$base_blk NEW_BLOCKS=$now_blk MEM_DETECT=$det MEMTOTAL_UP=$up" > "$R"
    sync; break
  fi
  sleep 1
done
POLL
  {
    echo "#cloud-config"
    echo "hostname: dlx-mem-proof"
    echo "package_update: false"
    echo "package_upgrade: false"
    echo "write_files:"
    echo "  - path: /usr/local/bin/mem-poll.sh"
    echo "    permissions: '0755'"
    echo "    encoding: b64"
    echo "    content: $(base64 -w0 "$MEMPOLL")"
    echo "runcmd:"
    echo "  - [ sh, -c, \"setsid bash /usr/local/bin/mem-poll.sh < /dev/null >/dev/null 2>&1 &\" ]"
  } > "$MEMUD"
  if check_then "vm create libvirt (hotplug-memory guest, headroom --memory-max 2G)" ok \
       "$BIN" vm create "$MEMVM" --disk "$MEMBASE" --backend libvirt --memory 1G --memory-max 2G --vcpus 2 --user-data "$MEMUD"; then
    MEMOVL="$SROOT/vms/$MEMVM.qcow2"
    # Dar tempo ao guest de arrancar e o cloud-init plantar o poller.
    sleep 100
    # NÍVEL 1: o provider aceita o pedido. A CLI sai 124 (`partial`) por falta de
    # agente; exige-se a MENSAGEM `memory -> 2048M` do provider, não o rc 0.
    check "nivel 1: o provider ACEITA a memoria a quente (attach-device DIMM live)" ok bash -c \
      "'$BIN' vm update '$MEMVM' --memory 2G --wait-secs 15 2>&1 | grep -Eq 'memory -> 2048M \\((partial|complete)\\)'"
    # NÍVEL 2: o DIMM quente está no domínio vivo (dumpxml lista-o).
    check "nivel 2: o DIMM quente aparece no dumpxml do domínio" ok bash -c \
      "virsh -c qemu:///system dumpxml '$MEMVM' 2>/dev/null | grep -q \"model='dimm'\""
    # Dar tempo ao poller in-guest de detectar + pôr online.
    sleep 40
    # Parar a VM: só com o qcow2 livre é que o guestfish lê o disco de arranque.
    "$BIN" vm stop "$MEMVM" >/dev/null 2>&1 || true
    for _ in $(seq 1 20); do
      virsh -c qemu:///system domstate "$MEMVM" 2>/dev/null | grep -q 'shut off' && break
      [[ -z "$(virsh -c qemu:///system domstate "$MEMVM" 2>/dev/null)" ]] && break
      sleep 2
    done
    # NÍVEL 3: o guest DETECTOU os blocos novos (apareceram acima do baseline).
    check "nivel 3: o guest DETECTA a memoria quente (novos blocos em sys-devices)" ok bash -c \
      "guestfish --ro -a '$MEMOVL' -i cat /var/lib/delonix-mem-RESULT | grep -q 'MEM_DETECT=ok'"
    # NÍVEL 4: a RAM quente está ONLINE e a contar — MemTotal subiu acima do boot.
    check "nivel 4: a memoria quente fica ONLINE DENTRO do guest (MemTotal sobe)" ok bash -c \
      "guestfish --ro -a '$MEMOVL' -i cat /var/lib/delonix-mem-RESULT | grep -q 'MEMTOTAL_UP=ok'"
    "$BIN" delete vm "$MEMVM" -f >/dev/null 2>&1
  else
    "$BIN" delete vm "$MEMVM" -f >/dev/null 2>&1
  fi
  rm -f "$MEMBASE" "$MEMUD" "$MEMPOLL"
else
  skip "vm: memoria a quente e online dentro do guest" \
    "sem libvirt utilizável, sem guestfish/cloud-localds, ou sem imagem cloud (E2E_CLOUDINIT_IMAGE)"
fi

########################################
section "vm: an extra disk declared at create is seen and R/W inside the guest (libvirt, cloud-init, guestfish)"
########################################
# `vm.disks.extra` on the libvirt provider was `partial` («`extraDisks` reach
# the domain XML (unit-tested target letters); never booted in the battery»).
# Reaching the XML proves the hypervisor accepts it; it does NOT prove the guest
# sees the disk nor that it is usable. This closes the gap live, against a real
# guest, for a disk declared AT CREATE (not hotplug — that is a sibling section):
#   1. APPLIED — the domain's `domblklist` lists the extra disk on a new target;
#   2. DETECTED — inside the guest a new block appears in `lsblk`/`/sys/block`;
#   3. USABLE — the guest WRITES a known marker to the raw block and READS it
#      back equal; the verdict lands in a file on root, read afterwards with
#      guestfish (the VM stopped, so QEMU releases the qcow2).
# The disk is declared through the engine's own declarative path (`vm apply` of
# a `kind: VirtualMachine` with `extraDisks:`), never hand-written XML. The extra
# disk is identified in-guest by its unique 128MiB size (root is 5G, the seed a
# few MB). Needs a cloud image (E2E_CLOUDINIT_IMAGE or one under
# $SROOT/vm-images/_base) and guestfish; without them, an honest SKIP.
XDIMG="${E2E_CLOUDINIT_IMAGE:-}"
if [[ -z "$XDIMG" ]]; then
  XDIMG="$(ls -1 "$SROOT"/vm-images/_base/*cloudimg*.img 2>/dev/null | head -1 || true)"
fi
if libvirt_system_usable && command -v guestfish >/dev/null 2>&1 && [[ -n "$XDIMG" && -r "$XDIMG" ]]; then
  XDVM="xdisk-$PFX"; XDBASE="$OUT/$XDVM-base.qcow2"; XDEXTRA="$OUT/$XDVM-extra.qcow2"
  XDUD="$OUT/$XDVM-user-data.yaml"; XDPOLL="$OUT/$XDVM-poll.sh"; XDMAN="$OUT/$XDVM.yaml"
  cp "$XDIMG" "$XDBASE" && qemu-img resize "$XDBASE" 5G >/dev/null 2>&1
  qemu-img create -f qcow2 "$XDEXTRA" 128M >/dev/null 2>&1
  cat > "$XDPOLL" <<'POLL'
#!/bin/bash
R=/var/lib/delonix-xdisk-RESULT; L=/var/lib/delonix-xdisk-LSBLK
exec >>/var/lib/delonix-xdisk-LOG 2>&1
for _ in $(seq 1 120); do
  dev=""
  while read -r name size; do [ "$size" = 134217728 ] && dev="$name" && break; done < <(lsblk -bdn -o NAME,SIZE)
  if [ -n "$dev" ]; then
    { echo "== lsblk =="; lsblk; echo "== /sys/block =="; ls -l /sys/block; } > "$L" 2>&1
    pat="EXTRADISK-BOOT-PROOF-v1"
    printf '%s\n' "$pat" | dd of="/dev/$dev" bs=512 seek=0 conv=notrunc 2>/dev/null
    blockdev --flushbufs "/dev/$dev" 2>/dev/null || true; sync
    rb="$(dd if=/dev/$dev bs=512 count=1 2>/dev/null | tr -d '\0' | head -c 32)"
    if [ "$rb" = "$pat" ]; then echo "DETECT=$dev WRITE=ok READ=ok MATCH=ok" > "$R"
    else echo "DETECT=$dev WRITE=sent READ=[$rb] MATCH=no" > "$R"; fi
    sync; break
  fi
  sleep 1
done
POLL
  {
    echo "#cloud-config"; echo "hostname: dlx-xdisk-proof"; echo "package_update: false"
    echo "write_files:"; echo "  - path: /usr/local/bin/xdisk-poll.sh"; echo "    permissions: '0755'"
    echo "    encoding: b64"; echo "    content: $(base64 -w0 "$XDPOLL")"
    echo "runcmd:"; echo "  - [ sh, -c, \"setsid bash /usr/local/bin/xdisk-poll.sh < /dev/null >/dev/null 2>&1 &\" ]"
  } > "$XDUD"
  cat > "$XDMAN" <<YAML
apiVersion: delonix.io/v1
kind: VirtualMachine
metadata: { name: $XDVM }
spec:
  disk: $XDBASE
  backend: libvirt
  memory: 1G
  vcpus: 2
  userData: $XDUD
  extraDisks:
    - source: $XDEXTRA
      bus: virtio
      format: qcow2
YAML
  if check_then "vm apply libvirt (extra-disk guest)" ok "$BIN" vm apply -f "$XDMAN"; then
    XDOVL="$SROOT/vms/$XDVM.qcow2"
    sleep 100
    # APPLIED: the extra disk is on the live domain, on a new target.
    check "the extra disk is in the domain's domblklist" ok bash -c \
      "virsh -c qemu:///system domblklist '$XDVM' | grep -q '$XDVM-extra.qcow2'"
    "$BIN" vm stop "$XDVM" >/dev/null 2>&1 || true
    for _ in $(seq 1 20); do
      virsh -c qemu:///system domstate "$XDVM" 2>/dev/null | grep -q 'shut off' && break
      [[ -z "$(virsh -c qemu:///system domstate "$XDVM" 2>/dev/null)" ]] && break
      sleep 2
    done
    # DETECTED: the guest saw the new 128MiB block (the poller recorded lsblk).
    check "the guest DETECTS the extra disk (lsblk/sys-block)" ok bash -c \
      "guestfish --ro -a '$XDOVL' -i cat /var/lib/delonix-xdisk-LSBLK | grep -Eq 'vd[b-z] +[0-9].* 128M'"
    # USABLE: a real write AND read to the extra disk inside the guest matched.
    check "the extra disk is R/W inside the guest (write+read match)" ok bash -c \
      "guestfish --ro -a '$XDOVL' -i cat /var/lib/delonix-xdisk-RESULT | grep -q 'WRITE=ok READ=ok MATCH=ok'"
    "$BIN" delete vm "$XDVM" -f >/dev/null 2>&1
  else
    "$BIN" delete vm "$XDVM" -f >/dev/null 2>&1
  fi
  rm -f "$XDBASE" "$XDEXTRA" "$XDUD" "$XDPOLL" "$XDMAN"
else
  skip "vm: an extra disk declared at create is seen and R/W inside the guest" \
    "no usable libvirt, no guestfish, or no cloud image (E2E_CLOUDINIT_IMAGE)"
fi

########################################
section "vm: an extra NIC declared at create is seen and passes real traffic inside the guest (libvirt, cloud-init, guestfish)"
########################################
# `vm.nics.extra` on the libvirt provider was `partial` («`extraNics`
# (network/bridge/user) reach the domain XML; never booted in the battery»).
# This closes it live, against a real guest, for a NIC declared AT CREATE (not
# hotplug — that is a sibling section):
#   1. APPLIED — the domain's `domiflist` lists the extra interface on `ingress`;
#   2. DETECTED — inside the guest a NIC appears in `/sys/class/net`;
#   3. REAL TRAFFIC — the guest configures the extra NIC (a static IP on the
#      `ingress` subnet) and pings the gateway (192.168.199.1); the verdict
#      records replies received and the rx/tx counter deltas, proof the NIC is
#      USABLE, not just present. Read afterwards with guestfish (VM stopped).
# The extra NIC is the physical interface WITHOUT a DHCP-assigned IPv4 (the
# primary gets one; the extra does not). Declared through `vm apply` of a
# `kind: VirtualMachine` with `extraNics:`. Needs a cloud image, guestfish and
# the libvirt `ingress` network; without them, an honest SKIP.
XNIMG="${E2E_CLOUDINIT_IMAGE:-}"
if [[ -z "$XNIMG" ]]; then
  XNIMG="$(ls -1 "$SROOT"/vm-images/_base/*cloudimg*.img 2>/dev/null | head -1 || true)"
fi
if libvirt_system_usable && command -v guestfish >/dev/null 2>&1 && [[ -n "$XNIMG" && -r "$XNIMG" ]] \
     && virsh -c qemu:///system net-info ingress >/dev/null 2>&1; then
  XNVM="xnic-$PFX"; XNBASE="$OUT/$XNVM-base.qcow2"
  XNUD="$OUT/$XNVM-user-data.yaml"; XNPOLL="$OUT/$XNVM-poll.sh"; XNMAN="$OUT/$XNVM.yaml"
  cp "$XNIMG" "$XNBASE" && qemu-img resize "$XNBASE" 5G >/dev/null 2>&1
  cat > "$XNPOLL" <<'POLL'
#!/bin/bash
R=/var/lib/delonix-xnic-RESULT; L=/var/lib/delonix-xnic-IPLINK
exec >>/var/lib/delonix-xnic-LOG 2>&1
for _ in $(seq 1 180); do
  dev=""
  for d in /sys/class/net/*; do
    n=$(basename "$d"); [ "$n" = lo ] && continue; [ -e "$d/device" ] || continue
    ip -4 addr show dev "$n" | grep -q 'inet ' && continue
    dev="$n"; break
  done
  if [ -n "$dev" ]; then
    { echo "== ip link =="; ip link; echo "== ip -4 addr =="; ip -4 addr; } > "$L" 2>&1
    ip link set "$dev" up; ip addr add 192.168.199.201/24 dev "$dev" 2>/dev/null; sleep 3
    rxb=$(cat "/sys/class/net/$dev/statistics/rx_packets"); txb=$(cat "/sys/class/net/$dev/statistics/tx_packets")
    ping -c 4 -W 2 -I "$dev" 192.168.199.1 > /var/lib/delonix-xnic-PING 2>&1
    rcv=$(grep -oE '[0-9]+ received' /var/lib/delonix-xnic-PING | grep -oE '^[0-9]+')
    rxa=$(cat "/sys/class/net/$dev/statistics/rx_packets"); txa=$(cat "/sys/class/net/$dev/statistics/tx_packets")
    rxd=$((rxa-rxb)); txd=$((txa-txb))
    if [ "${rcv:-0}" -ge 1 ]; then traf=ok-ping
    elif [ "$txd" -gt 0 ] && [ "$rxd" -gt 0 ]; then traf=ok-counters; else traf=no; fi
    echo "DETECT=$dev PING_RECV=${rcv:-0} RX_DELTA=$rxd TX_DELTA=$txd TRAFFIC=$traf" > "$R"
    sync; break
  fi
  sleep 1
done
POLL
  {
    echo "#cloud-config"; echo "hostname: dlx-xnic-proof"; echo "package_update: false"
    echo "write_files:"; echo "  - path: /usr/local/bin/xnic-poll.sh"; echo "    permissions: '0755'"
    echo "    encoding: b64"; echo "    content: $(base64 -w0 "$XNPOLL")"
    echo "runcmd:"; echo "  - [ sh, -c, \"setsid bash /usr/local/bin/xnic-poll.sh < /dev/null >/dev/null 2>&1 &\" ]"
  } > "$XNUD"
  cat > "$XNMAN" <<YAML
apiVersion: delonix.io/v1
kind: VirtualMachine
metadata: { name: $XNVM }
spec:
  disk: $XNBASE
  backend: libvirt
  memory: 1G
  vcpus: 2
  network: ingress
  userData: $XNUD
  extraNics:
    - type: network
      source: ingress
      model: virtio
YAML
  if check_then "vm apply libvirt (extra-nic guest)" ok "$BIN" vm apply -f "$XNMAN"; then
    XNOVL="$SROOT/vms/$XNVM.qcow2"
    sleep 100
    # APPLIED: the extra interface is on the live domain, on the ingress network.
    check "the extra NIC is in the domain's domiflist (ingress network)" ok bash -c \
      "virsh -c qemu:///system domiflist '$XNVM' | grep -qw ingress"
    "$BIN" vm stop "$XNVM" >/dev/null 2>&1 || true
    for _ in $(seq 1 20); do
      virsh -c qemu:///system domstate "$XNVM" 2>/dev/null | grep -q 'shut off' && break
      [[ -z "$(virsh -c qemu:///system domstate "$XNVM" 2>/dev/null)" ]] && break
      sleep 2
    done
    # DETECTED: the guest saw the extra NIC.
    check "the guest DETECTS the extra NIC (ip link/sys-class-net)" ok bash -c \
      "guestfish --ro -a '$XNOVL' -i cat /var/lib/delonix-xnic-RESULT | grep -Eq 'DETECT=[a-z0-9]+ '"
    # REAL TRAFFIC: ping to the gateway received end-to-end (or counters rose).
    check "the extra NIC passes real traffic inside the guest (ping/counters)" ok bash -c \
      "guestfish --ro -a '$XNOVL' -i cat /var/lib/delonix-xnic-RESULT | grep -Eq 'TRAFFIC=ok-(ping|counters)'"
    "$BIN" delete vm "$XNVM" -f >/dev/null 2>&1
  else
    "$BIN" delete vm "$XNVM" -f >/dev/null 2>&1
  fi
  rm -f "$XNBASE" "$XNUD" "$XNPOLL" "$XNMAN"
else
  skip "vm: an extra NIC declared at create is seen and passes real traffic inside the guest" \
    "no usable libvirt, no guestfish, no cloud image (E2E_CLOUDINIT_IMAGE), or no libvirt ingress network"
fi

########################################
section "vm: the requested CPU topology is seen inside the guest (libvirt, cloud-init, guestfish)"
########################################
# `vm.cpu.model` on the libvirt provider was `partial` («`cpuModel`/`cpuTopology`
# in the XML, default host-passthrough; not booted in the battery»). Reaching
# the XML proves nothing about what the GUEST sees. This boots a VM with a
# declared topology (sockets:2 cores:2 threads:1 = 4 vCPUs) and reads `lscpu`
# inside the guest: the guest must report EXACTLY the requested sockets/cores/
# threads. Declared through `vm apply` of a `kind: VirtualMachine` with
# `cpuModel:`/`cpuTopology:`. Needs a cloud image and guestfish; else SKIP.
XCIMG="${E2E_CLOUDINIT_IMAGE:-}"
if [[ -z "$XCIMG" ]]; then
  XCIMG="$(ls -1 "$SROOT"/vm-images/_base/*cloudimg*.img 2>/dev/null | head -1 || true)"
fi
if libvirt_system_usable && command -v guestfish >/dev/null 2>&1 && [[ -n "$XCIMG" && -r "$XCIMG" ]]; then
  XCVM="xcpu-$PFX"; XCBASE="$OUT/$XCVM-base.qcow2"
  XCUD="$OUT/$XCVM-user-data.yaml"; XCPOLL="$OUT/$XCVM-poll.sh"; XCMAN="$OUT/$XCVM.yaml"
  cp "$XCIMG" "$XCBASE" && qemu-img resize "$XCBASE" 5G >/dev/null 2>&1
  cat > "$XCPOLL" <<'POLL'
#!/bin/bash
R=/var/lib/delonix-xcpu-RESULT
exec >>/var/lib/delonix-xcpu-LOG 2>&1
for _ in $(seq 1 60); do
  sockets=$(lscpu | awk -F: '/^Socket\(s\)/{gsub(/ /,"",$2);print $2}')
  cores=$(lscpu | awk -F: '/Core\(s\) per socket/{gsub(/ /,"",$2);print $2}')
  threads=$(lscpu | awk -F: '/Thread\(s\) per core/{gsub(/ /,"",$2);print $2}')
  np=$(nproc)
  if [ -n "$sockets" ] && [ "$np" -ge 1 ]; then
    lscpu > /var/lib/delonix-xcpu-LSCPU 2>&1
    echo "SOCKETS=$sockets CORES_PER_SOCKET=$cores THREADS_PER_CORE=$threads NPROC=$np" > "$R"
    sync; break
  fi
  sleep 1
done
POLL
  {
    echo "#cloud-config"; echo "hostname: dlx-xcpu-proof"; echo "package_update: false"
    echo "write_files:"; echo "  - path: /usr/local/bin/xcpu-poll.sh"; echo "    permissions: '0755'"
    echo "    encoding: b64"; echo "    content: $(base64 -w0 "$XCPOLL")"
    echo "runcmd:"; echo "  - [ sh, -c, \"setsid bash /usr/local/bin/xcpu-poll.sh < /dev/null >/dev/null 2>&1 &\" ]"
  } > "$XCUD"
  cat > "$XCMAN" <<YAML
apiVersion: delonix.io/v1
kind: VirtualMachine
metadata: { name: $XCVM }
spec:
  disk: $XCBASE
  backend: libvirt
  memory: 1G
  vcpus: 4
  userData: $XCUD
  cpuModel: host-passthrough
  cpuTopology:
    sockets: 2
    cores: 2
    threads: 1
YAML
  if check_then "vm apply libvirt (cpu-topology guest)" ok "$BIN" vm apply -f "$XCMAN"; then
    XCOVL="$SROOT/vms/$XCVM.qcow2"
    sleep 100
    # APPLIED: the live domain counts 4 active vCPUs.
    check "the domain counts 4 live vCPUs (vcpucount)" ok bash -c \
      "[ \"\$(virsh -c qemu:///system vcpucount '$XCVM' --live --active 2>/dev/null | tr -d '[:space:]')\" = 4 ]"
    "$BIN" vm stop "$XCVM" >/dev/null 2>&1 || true
    for _ in $(seq 1 20); do
      virsh -c qemu:///system domstate "$XCVM" 2>/dev/null | grep -q 'shut off' && break
      [[ -z "$(virsh -c qemu:///system domstate "$XCVM" 2>/dev/null)" ]] && break
      sleep 2
    done
    # SEEN: the guest's lscpu reports EXACTLY the requested topology.
    check "the guest sees the requested topology (2 sockets, 2 cores, 1 thread)" ok bash -c \
      "guestfish --ro -a '$XCOVL' -i cat /var/lib/delonix-xcpu-RESULT | grep -q 'SOCKETS=2 CORES_PER_SOCKET=2 THREADS_PER_CORE=1 NPROC=4'"
    "$BIN" delete vm "$XCVM" -f >/dev/null 2>&1
  else
    "$BIN" delete vm "$XCVM" -f >/dev/null 2>&1
  fi
  rm -f "$XCBASE" "$XCUD" "$XCPOLL" "$XCMAN"
else
  skip "vm: the requested CPU topology is seen inside the guest" \
    "no usable libvirt, no guestfish, or no cloud image (E2E_CLOUDINIT_IMAGE)"
fi

########################################
section "vm: stop-then-start is a real reboot — the guest boot id changes (libvirt, cloud-init, guestfish)"
########################################
# `vm.restart` on the libvirt provider was `partial` («stop-then-start, always a
# real reboot; no battery check names it»). This names it: `vm restart` must be
# a REAL kernel reboot, not a resume. A cloud-init `bootcmd` (runs on EVERY boot)
# appends the kernel boot_id to a log; after `vm restart` the log holds TWO
# DISTINCT boot_ids — the kernel re-initialized, which is what a reboot is and a
# pause/resume is not. Needs a cloud image and guestfish; else an honest SKIP.
XRIMG="${E2E_CLOUDINIT_IMAGE:-}"
if [[ -z "$XRIMG" ]]; then
  XRIMG="$(ls -1 "$SROOT"/vm-images/_base/*cloudimg*.img 2>/dev/null | head -1 || true)"
fi
if libvirt_system_usable && command -v guestfish >/dev/null 2>&1 && [[ -n "$XRIMG" && -r "$XRIMG" ]]; then
  XRVM="xrst-$PFX"; XRBASE="$OUT/$XRVM-base.qcow2"; XRUD="$OUT/$XRVM-user-data.yaml"; XRMAN="$OUT/$XRVM.yaml"
  cp "$XRIMG" "$XRBASE" && qemu-img resize "$XRBASE" 5G >/dev/null 2>&1
  {
    echo "#cloud-config"; echo "hostname: dlx-xrst-proof"; echo "package_update: false"
    echo "bootcmd:"
    echo "  - [ sh, -c, \"cat /proc/sys/kernel/random/boot_id >> /var/lib/delonix-xrst-BOOTLOG\" ]"
  } > "$XRUD"
  cat > "$XRMAN" <<YAML
apiVersion: delonix.io/v1
kind: VirtualMachine
metadata: { name: $XRVM }
spec:
  disk: $XRBASE
  backend: libvirt
  memory: 1G
  vcpus: 2
  userData: $XRUD
YAML
  if check_then "vm apply libvirt (restart guest)" ok "$BIN" vm apply -f "$XRMAN"; then
    XROVL="$SROOT/vms/$XRVM.qcow2"
    sleep 100
    # The real reboot under test.
    check "vm restart returns ok" ok "$BIN" vm restart "$XRVM"
    sleep 70
    "$BIN" vm stop "$XRVM" >/dev/null 2>&1 || true
    for _ in $(seq 1 20); do
      virsh -c qemu:///system domstate "$XRVM" 2>/dev/null | grep -q 'shut off' && break
      [[ -z "$(virsh -c qemu:///system domstate "$XRVM" 2>/dev/null)" ]] && break
      sleep 2
    done
    # PROOF: two DISTINCT boot_ids in the log — the kernel re-initialized.
    check "the guest boot id changed across the restart (two distinct ids)" ok bash -c \
      "[ \"\$(guestfish --ro -a '$XROVL' -i cat /var/lib/delonix-xrst-BOOTLOG 2>/dev/null | grep -c '[0-9a-f]')\" -ge 2 ] && [ \"\$(guestfish --ro -a '$XROVL' -i cat /var/lib/delonix-xrst-BOOTLOG 2>/dev/null | sort -u | grep -c '[0-9a-f]')\" -ge 2 ]"
    "$BIN" delete vm "$XRVM" -f >/dev/null 2>&1
  else
    "$BIN" delete vm "$XRVM" -f >/dev/null 2>&1
  fi
  rm -f "$XRBASE" "$XRUD" "$XRMAN"
else
  skip "vm: stop-then-start is a real reboot — the guest boot id changes" \
    "no usable libvirt, no guestfish, or no cloud image (E2E_CLOUDINIT_IMAGE)"
fi

########################################
section "vm: a stopped VM cold-migrates — flatten, import, boot on the target with state intact (libvirt, guestfish)"
########################################
# `vm.migration.cold` on the libvirt provider was `partial` («`vm migrate`: stop,
# flatten, copy over SSH, import, start; no battery»). This proves the mechanism
# live on ONE host — the substantive, risky steps of cold migration, minus only
# the generic scp transport between two hosts (not a VM-provider capability):
#   1. STOP + FLATTEN — `qemu-img convert -O qcow2` produces a STANDALONE disk
#      with no backing-file dependency to carry across hosts;
#   2. IMPORT --appliance — the carried disk is registered WITHOUT a fresh seed,
#      so the guest's baked state is not reset (ADR: an appliance keeps its own);
#   3. CREATE on the "target" (a second name here = the other host) and BOOT it.
# A BAKED systemd unit (installed once by cloud-init, then the guest's own, so it
# survives the seedless appliance import) appends the boot_id on every boot.
# Proof: the target's log holds the source's first-boot id (state crossed intact)
# AND a second, distinct id (the target actually booted the migrated disk). Needs
# a cloud image and guestfish; else an honest SKIP.
XMIMG="${E2E_CLOUDINIT_IMAGE:-}"
if [[ -z "$XMIMG" ]]; then
  XMIMG="$(ls -1 "$SROOT"/vm-images/_base/*cloudimg*.img 2>/dev/null | head -1 || true)"
fi
if libvirt_system_usable && command -v guestfish >/dev/null 2>&1 && [[ -n "$XMIMG" && -r "$XMIMG" ]]; then
  XMSRC="mgsrc-$PFX"; XMDST="mgdst-$PFX"; XMTAG="migrated-$XMSRC"
  XMBASE="$OUT/$XMSRC-base.qcow2"; XMFLAT="$OUT/$XMSRC-flat.qcow2"; XMUD="$OUT/$XMSRC-user-data.yaml"; XMMAN="$OUT/$XMSRC.yaml"
  cp "$XMIMG" "$XMBASE" && qemu-img resize "$XMBASE" 5G >/dev/null 2>&1
  {
    echo "#cloud-config"; echo "hostname: dlx-mig-proof"; echo "package_update: false"
    echo "write_files:"
    echo "  - path: /etc/systemd/system/dlx-bootlog.service"
    echo "    permissions: '0644'"
    echo "    content: |"
    echo "      [Unit]"
    echo "      Description=delonix migration boot logger"
    echo "      After=local-fs.target"
    echo "      [Service]"
    echo "      Type=oneshot"
    echo "      ExecStart=/bin/sh -c 'cat /proc/sys/kernel/random/boot_id >> /var/lib/delonix-mig-BOOTLOG'"
    echo "      [Install]"
    echo "      WantedBy=multi-user.target"
    echo "runcmd:"
    echo "  - [ systemctl, enable, dlx-bootlog.service ]"
    echo "  - [ systemctl, start, dlx-bootlog.service ]"
  } > "$XMUD"
  cat > "$XMMAN" <<YAML
apiVersion: delonix.io/v1
kind: VirtualMachine
metadata: { name: $XMSRC }
spec:
  disk: $XMBASE
  backend: libvirt
  memory: 1G
  vcpus: 2
  userData: $XMUD
YAML
  if check_then "vm apply libvirt (cold-migrate source)" ok "$BIN" vm apply -f "$XMMAN"; then
    XMSOVL="$SROOT/vms/$XMSRC.qcow2"
    sleep 100
    "$BIN" vm stop "$XMSRC" >/dev/null 2>&1 || true
    for _ in $(seq 1 20); do
      virsh -c qemu:///system domstate "$XMSRC" 2>/dev/null | grep -q 'shut off' && break
      [[ -z "$(virsh -c qemu:///system domstate "$XMSRC" 2>/dev/null)" ]] && break
      sleep 2
    done
    # STEP 1: flatten — the standalone disk carries no backing dependency.
    check "cold-migrate: the flattened disk is standalone (no backing file)" ok bash -c \
      "qemu-img convert -O qcow2 -- '$XMSOVL' '$XMFLAT' && ! qemu-img info '$XMFLAT' | grep -qi 'backing file'"
    # STEP 2+3: import --appliance, then create+boot on the 'target'.
    check "cold-migrate: the carried disk imports as an appliance" ok \
      "$BIN" vm image import "$XMFLAT" --tag "$XMTAG" --appliance --force
    check "cold-migrate: the target VM creates+boots from the migrated disk" ok \
      "$BIN" vm create "$XMDST" --disk "$XMTAG" --vcpus 2 --memory 1G --network ingress --backend libvirt
    sleep 100
    XMDOVL="$SROOT/vms/$XMDST.qcow2"
    "$BIN" vm stop "$XMDST" >/dev/null 2>&1 || true
    for _ in $(seq 1 20); do
      virsh -c qemu:///system domstate "$XMDST" 2>/dev/null | grep -q 'shut off' && break
      [[ -z "$(virsh -c qemu:///system domstate "$XMDST" 2>/dev/null)" ]] && break
      sleep 2
    done
    # PROOF: the target's bootlog carries the source's first-boot id AND a second,
    # distinct one — state crossed intact and the target booted the migrated disk.
    check "cold-migrate: the target booted the migrated disk with state intact (two distinct boot ids)" ok bash -c \
      "[ \"\$(guestfish --ro -a '$XMDOVL' -i cat /var/lib/delonix-mig-BOOTLOG 2>/dev/null | sort -u | grep -c '[0-9a-f]')\" -ge 2 ]"
    check "cold-migrate: the target kept the source's baked hostname" ok bash -c \
      "guestfish --ro -a '$XMDOVL' -i cat /etc/hostname | grep -qx dlx-mig-proof"
    "$BIN" delete vm "$XMDST" -f >/dev/null 2>&1
    "$BIN" vm image rm "$XMTAG" -f >/dev/null 2>&1 || "$BIN" vm image rm "$XMTAG" >/dev/null 2>&1
    "$BIN" delete vm "$XMSRC" -f >/dev/null 2>&1
  else
    "$BIN" delete vm "$XMSRC" -f >/dev/null 2>&1
  fi
  rm -f "$XMBASE" "$XMFLAT" "$XMUD" "$XMMAN"
else
  skip "vm: a stopped VM cold-migrates — flatten, import, boot on the target with state intact" \
    "no usable libvirt, no guestfish, or no cloud image (E2E_CLOUDINIT_IMAGE)"
fi

########################################
section "vm: the serial console connects and VNC answers RFB, live (libvirt, cloud-init)"
########################################
# §7 of the mandate: `vm.console.serial` and `vm.console.vnc` on the libvirt
# provider were `partial` («runs `virsh console --force`; interactive, no
# battery» / «reads `vncdisplay`; no battery»). A command that EXISTS is not a
# console that WORKS: the serial socket could fail to carry a single byte of the
# guest, and `vm vnc` could print a display nobody is listening on. Both holes
# are closed here, live, against a REAL cloud-init guest booted through the
# engine's own declarative path (created with `--vnc`):
#   VNC   — `vm vnc` names a VNC endpoint, and a raw TCP connection to that port
#           reads back QEMU's `RFB 003.00x` protocol handshake. The server is
#           listening and speaks the protocol — a client CAN connect, not just a
#           number got printed. (`vm vnc` prints the virsh display form
#           `host:N`; N < 1000 is a VNC display index, so the TCP port is
#           5900+N — the same resolution any VNC client does.)
#   SERIAL— `vm console` (which runs `virsh console --force` over the serial
#           pty) is driven non-interactively under a pseudo-terminal (`script`)
#           with a timeout, and the capture carries the GUEST'S OWN getty login
#           prompt — the hostname cloud-init set, followed by `login:`. Bytes
#           from the guest crossed the serial socket into `vm console`: the
#           console FUNCTIONS, it is not merely wired.
# Needs a cloud image (E2E_CLOUDINIT_IMAGE or one under $SROOT/vm-images/_base),
# `cloud-localds` and `script` (util-linux). Without them, an honest SKIP — the
# CI battery carries no boot image.
CVIMG="${E2E_CLOUDINIT_IMAGE:-}"
if [[ -z "$CVIMG" ]]; then
  CVIMG="$(ls -1 "$SROOT"/vm-images/_base/*cloudimg*.img 2>/dev/null | head -1 || true)"
fi
if libvirt_system_usable && command -v script >/dev/null 2>&1 \
     && command -v cloud-localds >/dev/null 2>&1 && [[ -n "$CVIMG" && -r "$CVIMG" ]]; then
  CVVM="convnc-$PFX"; CVBASE="$OUT/$CVVM-base.qcow2"; CVUD="$OUT/$CVVM-user-data.yaml"
  CVLOG="$OUT/$CVVM-console.log"
  cp "$CVIMG" "$CVBASE" && qemu-img resize "$CVBASE" 5G >/dev/null 2>&1
  cat > "$CVUD" <<'YAML'
#cloud-config
hostname: dlx-console-proof
package_update: false
package_upgrade: false
YAML
  if check_then "vm create libvirt (console/vnc guest)" ok \
       "$BIN" vm create "$CVVM" --disk "$CVBASE" --backend libvirt --memory 1G --vcpus 2 --vnc --user-data "$CVUD"; then
    # VNC first: QEMU's VNC server listens as soon as the domain starts, so this
    # needs no boot. Resolve the port from whatever `vm vnc` prints (a `host:N`
    # display index or a `host:port`), connect, and read the RFB handshake.
    check "vm vnc names a VNC endpoint that answers the RFB protocol handshake" ok bash -c "
      for _ in \$(seq 1 15); do
        addr=\$('$BIN' vm vnc '$CVVM' 2>/dev/null | head -1)
        [ -n \"\$addr\" ] || { sleep 1; continue; }
        host=\${addr%:*}; n=\${addr##*:}
        if [ \"\$n\" -lt 1000 ] 2>/dev/null; then port=\$((5900+n)); else port=\$n; fi
        hs=\$(timeout 5 bash -c \"exec 3<>/dev/tcp/\$host/\$port; head -c 12 <&3\" 2>/dev/null)
        case \"\$hs\" in RFB*) exit 0;; esac
        sleep 1
      done
      exit 1"
    # Give the guest time to reach the serial getty, then drive `vm console`
    # non-interactively under a pty and read back the guest's OWN login prompt.
    sleep 95
    timeout 25 script -q -c "'$BIN' vm console '$CVVM'" "$CVLOG" </dev/null >/dev/null 2>&1 || true
    check "vm console returns the guest's own login prompt over the serial socket" ok bash -c \
      "grep -aq 'dlx-console-proof login:' '$CVLOG'"
    "$BIN" delete vm "$CVVM" -f >/dev/null 2>&1
  else
    "$BIN" delete vm "$CVVM" -f >/dev/null 2>&1
  fi
  rm -f "$CVBASE" "$CVUD" "$CVLOG"
else
  skip "vm: serial console connects and VNC answers RFB" \
    "no usable libvirt, no cloud image (E2E_CLOUDINIT_IMAGE), or no script/cloud-localds"
fi

########################################
section "vm: backup create archives a live guest and restore RECOVERS disk data (libvirt, guestfish)"
########################################
# §7 of the mandate: `vm.backup.disk` and `vm.backup.restore` on the libvirt
# provider were `partial` («the battery backs up a container, not a VM»). A
# `backup create` that returns 0 proves nothing — the archive could hold a torn
# or empty disk, and `restore` could transition state without putting a single
# byte back. Both holes are closed here, live, against a REAL cloud-init guest,
# the same DATA-recovery discipline the snapshot section uses (criterion #16):
#   1. the guest's cloud-init writes a known marker (ORIGINAL) to its own disk;
#   2. `backup create vm` runs while the guest is RUNNING — exercising libvirt's
#      external-snapshot + `blockcommit --active --pivot` dance — and the guest
#      SURVIVES it (same state, same qemu pid): the live backup did not stop it
#      (= vm.backup.disk);
#   3. the guest is stopped (the qemu holds the qcow2 while it runs) and the disk
#      is read with guestfish — the archive carried the guest's OWN data;
#   4. the marker is CORRUPTED on the disk;
#   5. `backup restore` re-imports the archived disk;
#   6. the disk is re-read — the ORIGINAL marker is BACK. Data recovered, not an
#      exit code (= vm.backup.restore).
# Quiesce is deliberately NOT promoted here: `--quiesce` needs a qemu-guest-agent
# running inside the guest, which this battery cannot install offline — it stays
# an honest `partial`.
# Needs a cloud image, guestfish and cloud-localds; else an honest SKIP.
BVIMG="${E2E_CLOUDINIT_IMAGE:-}"
if [[ -z "$BVIMG" ]]; then
  BVIMG="$(ls -1 "$SROOT"/vm-images/_base/*cloudimg*.img 2>/dev/null | head -1 || true)"
fi
if libvirt_system_usable && command -v guestfish >/dev/null 2>&1 \
     && command -v cloud-localds >/dev/null 2>&1 && [[ -n "$BVIMG" && -r "$BVIMG" ]]; then
  BVVM="bkvm-$PFX"; BVBASE="$OUT/$BVVM-base.qcow2"; BVUD="$OUT/$BVVM-user-data.yaml"
  BVDIR="$OUT/vmbackups-$PFX"; rm -rf "$BVDIR"; mkdir -p "$BVDIR"
  cp "$BVIMG" "$BVBASE" && qemu-img resize "$BVBASE" 5G >/dev/null 2>&1
  cat > "$BVUD" <<'YAML'
#cloud-config
hostname: dlx-vmbackup-proof
package_update: false
package_upgrade: false
write_files:
  - path: /var/lib/delonix-vmbackup-OK
    permissions: '0644'
    content: |
      VMBACKUP-ORIGINAL-v1
YAML
  if check_then "vm create libvirt (vm-backup guest)" ok \
       "$BIN" vm create "$BVVM" --disk "$BVBASE" --backend libvirt --memory 1G --vcpus 2 --user-data "$BVUD"; then
    BVOVL="$SROOT/vms/$BVVM.qcow2"
    # Let cloud-init write the marker (write_files runs early in boot).
    sleep 100
    # vm.backup.disk: back up the RUNNING guest and watch it survive. The qemu
    # pid before == after proves the external-snapshot + blockcommit left the
    # live guest in place (it was not stopped under the hood).
    BVPID="$(pgrep -f "guest=$BVVM" | head -1)"
    check "backup create vm archives a RUNNING guest and it survives (live snapshot + blockcommit)" ok bash -c "
      '$BIN' backup create vm '$BVVM' --to '$BVDIR' >/dev/null 2>&1 || exit 1
      ls '$BVDIR'/vm-$BVVM-*.tar.gz >/dev/null 2>&1 || exit 1
      [ \"\$(virsh -c qemu:///system domstate '$BVVM' 2>/dev/null)\" = running ] || exit 1
      [ -n '$BVPID' ] && [ \"\$(pgrep -f 'guest=$BVVM' | head -1)\" = '$BVPID' ]"
    # Stop the guest so the qcow2 is free for guestfish.
    "$BIN" vm stop "$BVVM" >/dev/null 2>&1 || true
    for _ in $(seq 1 20); do
      virsh -c qemu:///system domstate "$BVVM" 2>/dev/null | grep -q 'shut off' && break
      [[ -z "$(virsh -c qemu:///system domstate "$BVVM" 2>/dev/null)" ]] && break
      sleep 2
    done
    # The archive carried the guest's OWN data (ORIGINAL is on the disk it copied).
    check "the overlay the live backup copied holds the guest's ORIGINAL data" ok bash -c \
      "guestfish --ro -a '$BVOVL' -i cat /var/lib/delonix-vmbackup-OK | grep -qx VMBACKUP-ORIGINAL-v1"
    # Corrupt it on the disk, with the guest stopped (the qcow2 is free).
    check "corrupt the marker on the VM disk" ok bash -c \
      "guestfish -a '$BVOVL' -i write /var/lib/delonix-vmbackup-OK VMBACKUP-CORRUPTED-v2"
    check "the VM disk now reads CORRUPTED" ok bash -c \
      "guestfish --ro -a '$BVOVL' -i cat /var/lib/delonix-vmbackup-OK | grep -qx VMBACKUP-CORRUPTED-v2"
    # vm.backup.restore: re-import the archived disk.
    check "backup restore re-imports the VM disk" ok bash -c \
      "'$BIN' backup restore \$(ls '$BVDIR'/vm-$BVVM-*.tar.gz | head -1)"
    # THE PROOF: the ORIGINAL marker is back — data recovered, not an exit code.
    check "backup restore RECOVERS the VM disk data (not just an exit code)" ok bash -c \
      "guestfish --ro -a '$BVOVL' -i cat /var/lib/delonix-vmbackup-OK | grep -qx VMBACKUP-ORIGINAL-v1"
    "$BIN" delete vm "$BVVM" -f >/dev/null 2>&1
  else
    "$BIN" delete vm "$BVVM" -f >/dev/null 2>&1
  fi
  rm -rf "$BVDIR"; rm -f "$BVBASE" "$BVUD"
else
  skip "vm: backup archives a live guest and restore recovers disk data" \
    "no usable libvirt, no guestfish/cloud-localds, or no cloud image (E2E_CLOUDINIT_IMAGE)"
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
# dentro). Salta só quando o host não tem o CH (`ch_usable`); um `create` que
# falha num host que o tem é FAIL — o holder é coisa do motor, não do host.
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
elif ch_usable; then
  CVM="chsnap-$PFX"; CDISK="$OUT/$CVM.qcow2"
  qemu-img create -f qcow2 "$CDISK" 64M >/dev/null 2>&1
  if check_then "vm create cloud-hypervisor (snapshot)" ok \
       "$BIN" vm create "$CVM" --disk "$CDISK" --backend cloud-hypervisor --memory 256M; then
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
    # D6 (docs/discovery/65_PLANO_MATURIDADE.md): a `--wait` that runs out of
    # time is an ERROR. The disk is empty, so the guest never answers; the
    # create must say so with exit 124 (DX-8503) and leave the VM running.
    # Before, it printed a warning and exited 0 — a script's next step ran
    # against a guest that never booted.
    WVM="$CVM-w"
    check "CH: vm create --wait que esgota o tempo sai com 124" 124 \
      "$BIN" vm create "$WVM" --disk "$CDISK" --backend cloud-hypervisor --memory 256M \
      --wait --boot-timeout 5
    check "CH: e a VM fica a correr" ok bash -c \
      "'$BIN' vm ls -o json | python3 -c \"import json,sys; sys.exit(0 if any(v['name']=='$WVM' and v['status']=='Running' for v in json.load(sys.stdin)) else 1)\""
    "$BIN" delete vm "$WVM" -f >/dev/null 2>&1
  else
    "$BIN" delete vm "$CVM" -f >/dev/null 2>&1
  fi
  rm -f "$CDISK"
else
  skip "vm: snapshots no cloud-hypervisor" "$(ch_missing)"
fi

########################################
section "vm: ciclo de vida no cloud-hypervisor (pause/resume/restart/resize.cold/destroy)"
########################################
# MESMA convenção das secções CH acima: prova o MECANISMO do motor contra um
# disco VAZIO (o VMM sobe e fica em `S` no firmware à espera de um device de
# arranque — `vm ls` di-lo Running), sem um SO convidado. O que é a nível de
# GUEST — o convidado volta depois de `pause`/`restart`, vê os vCPUs novos após
# `resize`, e nada fica órfão — foi validado AO VIVO ponta-a-ponta com a golden
# `k8s-golden` (login real por `delonix vm ssh`, 28/28 asserções) e registado em
# `reports/vm-native-ch-audit-2026-10-24/` + o arnês reproduzível
# `scripts/vm-native-ch-lifecycle.sh`; não se repete aqui porque exigiria puxar
# uma imagem por corrida da bateria sem provar nada que este mecanismo já não
# prove. É esta a evidência que gradua estas capacidades de `partial`.
if [[ -n "${DELONIX_ROOT:-}" && -z "${DELONIX_NET_RUNTIME_DIR:-}" ]]; then
  skip "vm: ciclo de vida no cloud-hypervisor" \
    "DELONIX_ROOT isolado sem DELONIX_NET_RUNTIME_DIR — isola os DOIS ou nenhum"
elif ch_usable; then
  LVM="chlife-$PFX"; LDISK="$OUT/$LVM.qcow2"; LSOCK="$SROOT/vms/$LVM.sock"
  qemu-img create -f qcow2 "$LDISK" 64M >/dev/null 2>&1
  if check_then "vm create cloud-hypervisor (ciclo de vida)" ok \
       "$BIN" vm create "$LVM" --disk "$LDISK" --backend cloud-hypervisor --memory 256M; then
    # pause/resume: PUT /api/v1/vm.{pause,resume} no socket do VMM; o VMM
    # continua de pé dos dois lados (vm.resume)
    check "CH: vm pause" ok "$BIN" vm pause "$LVM"
    check "CH: o VMM continua vivo depois do pause" ok bash -c "pgrep -f -- '--api-socket $LSOCK( |\$)' >/dev/null"
    check "CH: vm resume" ok "$BIN" vm unpause "$LVM"
    check "CH: o VMM continua vivo depois do resume" ok bash -c "pgrep -f -- '--api-socket $LSOCK( |\$)' >/dev/null"
    # restart: stop-then-start — reboot real; o VMM sai e volta a subir
    check "CH: vm restart" ok "$BIN" vm restart "$LVM"
    check "CH: o VMM está de pé depois do restart" ok bash -c "pgrep -f -- '--api-socket $LSOCK( |\$)' >/dev/null"
    # resize.cold: `vm resize` reescreve os vcpus no record e `vm start`
    # reconstrói a linha de comando do VMM a partir dele — a testemunha
    # independente é o argv do processo CH (boot=2)
    check "CH: vm stop (resize.cold)" ok "$BIN" vm stop "$LVM"
    check "CH: vm resize --vcpus 2 com a VM parada" ok "$BIN" vm resize "$LVM" --vcpus 2
    check "CH: vm start depois do resize" ok "$BIN" vm start "$LVM"
    check "CH: o VMM arranca com boot=2 vCPUs" ok bash -c \
      "pgrep -af -- '--api-socket $LSOCK' | grep -q -- '--cpus boot=2'"
    # destroy: termina o VMM (guarda pid+starttime) e remove tudo o que é da VM
    check "CH: vm destroy" ok "$BIN" delete vm "$LVM" -f
    check "CH: destroy não deixa o VMM órfão" ok bash -c "! pgrep -f -- '--api-socket $LSOCK( |\$)' >/dev/null"
    check "CH: destroy não deixa o record" ok bash -c \
      "! '$BIN' vm ls -o json | python3 -c \"import json,sys; sys.exit(0 if any(v['name']=='$LVM' for v in json.load(sys.stdin)) else 1)\""
    check "CH: destroy não deixa ficheiros no vms/" ok bash -c \
      "! ls '$SROOT'/vms/'$LVM'.* >/dev/null 2>&1"
  else
    "$BIN" delete vm "$LVM" -f >/dev/null 2>&1
  fi
  rm -f "$LDISK"
else
  skip "vm: ciclo de vida no cloud-hypervisor" "$(ch_missing)"
fi

########################################
section "vm: --disk-size cresce o disco de arranque, só a crescer (vm.disk.resize)"
########################################
# Achado e corrigido a auditar a prontidão VMaaS (docs/discovery/67): esta
# capacidade estava not-implemented nos três backends. O que se prova AQUI é o
# MECANISMO do motor (cresce o qcow2, recusa um encolher, recusa com a VM a
# correr) contra um disco VAZIO — a mesma convenção das secções de snapshot
# acima: nada disto precisa de um SO convidado a sério. Que um convidado com
# `growpart`/cloud-init apanha o espaço novo sozinho no arranque seguinte foi
# validado ao vivo, à mão, com a golden debian-bookworm — registado no
# discovery doc, não repetido aqui (pedir um pull de imagem a cada corrida da
# bateria custaria minutos por nada que este mecanismo já não prove).
if libvirt_system_usable; then
  RVM="resize-$PFX"; RDISK="$OUT/$RVM.qcow2"
  qemu-img create -f qcow2 "$RDISK" 64M >/dev/null 2>&1
  if check_then "vm create libvirt (disk resize)" ok \
       "$BIN" vm create "$RVM" --disk "$RDISK" --backend libvirt --memory 256M; then
    check "vm resize --disk-size com a VM a correr recusa (5)" 5 "$BIN" vm resize "$RVM" --disk-size 1
    check "vm stop (disk resize)" ok "$BIN" vm stop "$RVM"
    check "vm resize --disk-size 0 recusa (1)" 1 "$BIN" vm resize "$RVM" --disk-size 0
    # O disco vazio tem 64 MiB — bem abaixo de 1 GiB, por isso é sempre um grow.
    check "vm resize --disk-size de uma VM parada" ok "$BIN" vm resize "$RVM" --disk-size 1
    check "o overlay cresceu para exactamente 1 GiB" ok bash -c \
      "qemu-img info --output=json '$SROOT/vms/$RVM.qcow2' | python3 -c \"import json,sys; sys.exit(0 if json.load(sys.stdin)['virtual-size']==1073741824 else 1)\""
    check "um --disk-size que não cresce recusa (1)" 1 "$BIN" vm resize "$RVM" --disk-size 1
    check "vm start depois do resize do disco" ok "$BIN" vm start "$RVM"
    check "o domínio continua a correr com o disco maior" ok bash -c \
      "[ \"\$(virsh -c qemu:///system domstate '$RVM')\" = running ]"
    "$BIN" delete vm "$RVM" -f >/dev/null 2>&1
  else
    "$BIN" delete vm "$RVM" -f >/dev/null 2>&1
  fi
  rm -f "$RDISK"
else
  skip "vm: --disk-size (libvirt)" "sem virsh/qemu-img/qemu-system-x86_64, ou sem ligação libvirt de sistema"
fi

# The NODE API's own create on a network the node declared and libvirt does
# not know yet (cli-tester, 2026-10-21): `config_from_spec` left the subnet
# out, the provider had nothing to define the libvirt network from, and the
# operation FAILED with DX-9501 "no network with matching name" — while the
# CLI, on the same input, worked. What is read is the libvirt network itself.
# The subnet is picked off the host's routes, and the only libvirt network
# removed is the one this block saw absent and then created.
if libvirt_system_usable && [[ -x "$NODEBIN" ]]; then
  NVNET="nv-$PFX"; NVVM="nvvm-$PFX"; NVDISK="$OUT/$NVVM.qcow2"; NVOCT=""
  # The walk starts at an offset of this run's PID, so two batteries in
  # parallel do not both pick the first free octet before either one's
  # libvirt network has a route (measured: the second define is refused).
  for _i in $(seq 0 230); do
    _o=$(( 20 + ($$ + _i) % 231 ))
    ip -4 route | grep -q "^10\.237\.$_o\." || { NVOCT=$_o; break; }
  done
  if [[ -z "$NVOCT" ]]; then
    skip "node API: VM libvirt numa rede declarada" "nenhum 10.237.N.0/24 livre nas rotas deste host"
  elif virsh -c qemu:///system net-info "$NVNET" >/dev/null 2>&1; then
    skip "node API: VM libvirt numa rede declarada" "a rede libvirt $NVNET já existe — não é deste teste"
  else
    qemu-img create -f qcow2 "$NVDISK" 64M >/dev/null 2>&1
    "$BIN" serve node-api --addr "unix://$NODESOCK" >>"$OUT/node-api.log" 2>&1 &
    NODEPID=$!
    for _ in $(seq 100); do [[ -S "$NODESOCK" ]] && break; sleep 0.05; done
    if check_then "network create --subnet de uma rede para uma VM libvirt da API de nó" ok \
         "$BIN" network create "$NVNET" --subnet "10.237.$NVOCT.0/24"; then
      check "node API: CreateVirtualMachine libvirt numa rede declarada sem rede libvirt prévia termina SUCCEEDED" ok \
        env NODESOCK="$NODESOCK" NVVM="$NVVM" NVNET="$NVNET" NVDISK="$NVDISK" python3 -c '
import json,os,subprocess,time
def call(method, path, body=None):
    cmd=["curl","-s","-X",method,"--unix-socket",os.environ["NODESOCK"]]
    if body is not None: cmd+=["-H","content-type: application/json","-d",json.dumps(body)]
    return json.loads(subprocess.run(cmd+["http://localhost"+path],capture_output=True,text=True).stdout)
op=call("POST","/v1/namespaces/default/virtualmachines",{"name":os.environ["NVVM"],"spec":{
    "image":os.environ["NVDISK"],"provider":"libvirt","memory_bytes":268435456,
    "networks":[{"network":os.environ["NVNET"]}]}})
assert "id" in op, op
deadline=time.time()+180
while op.get("state") in ("OPERATION_STATE_PENDING","OPERATION_STATE_RUNNING") and time.time()<deadline:
    time.sleep(1); op=call("GET","/v1/operations/"+op["id"])
assert op.get("state")=="OPERATION_STATE_SUCCEEDED", op'
      check "node API: a rede libvirt nasce com a sub-rede declarada (virsh net-dumpxml)" ok bash -c \
        "virsh -c qemu:///system net-dumpxml '$NVNET' | grep -q \"address='10.237.$NVOCT.1' netmask='255.255.255.0'\""
      # The node API's DELETE of the network's last VM takes the libvirt
      # network with it — the release the use case owns. Measured through the
      # API because that is where it stayed behind (cli-tester, 2026-10-21).
      check "node API: o DELETE da última VM leva a rede libvirt que o motor definiu" ok \
        env NODESOCK="$NODESOCK" NVVM="$NVVM" NVNET="$NVNET" python3 -c '
import json,os,subprocess,time
def call(method, path):
    out=subprocess.run(["curl","-s","-X",method,"--unix-socket",os.environ["NODESOCK"],"http://localhost"+path],capture_output=True,text=True).stdout
    return json.loads(out)
op=call("DELETE","/v1/namespaces/default/virtualmachines/"+os.environ["NVVM"])
assert "id" in op, op
deadline=time.time()+120
while op.get("state") in ("OPERATION_STATE_PENDING","OPERATION_STATE_RUNNING") and time.time()<deadline:
    time.sleep(1); op=call("GET","/v1/operations/"+op["id"])
assert op.get("state")=="OPERATION_STATE_SUCCEEDED", op
gone=subprocess.run(["virsh","-c","qemu:///system","net-info",os.environ["NVNET"]],capture_output=True).returncode!=0
assert gone, "the libvirt network outlived its last VM"'
    fi
    "$BIN" delete vm "$NVVM" -f >/dev/null 2>&1
    kill "$NODEPID" 2>/dev/null; wait "$NODEPID" 2>/dev/null; rm -f "$NODESOCK"
    # Only a safety net: the check above is what proves the release. A FAIL
    # there must not leave this block's network on the host, and it is still
    # the one this block saw absent and then created.
    if virsh -c qemu:///system net-info "$NVNET" >/dev/null 2>&1; then
      virsh -c qemu:///system net-destroy "$NVNET" >/dev/null 2>&1
      virsh -c qemu:///system net-undefine "$NVNET" >/dev/null 2>&1
    fi
    e2e_net_rm "$NVNET"
    rm -f "$NVDISK"
  fi
  unset _o _i
else
  skip "node API: VM libvirt numa rede declarada" "sem libvirt de sistema utilizável ou sem delonix-node-api"
fi

# Mesma ressalva de isolamento da secção CH acima: meia-isolação é pior que
# nenhuma.
if [[ -n "${DELONIX_ROOT:-}" && -z "${DELONIX_NET_RUNTIME_DIR:-}" ]]; then
  skip "vm: --disk-size (cloud-hypervisor)" \
    "DELONIX_ROOT isolado sem DELONIX_NET_RUNTIME_DIR — isola os DOIS ou nenhum"
elif ch_usable; then
  RCVM="chresize-$PFX"; RCDISK="$OUT/$RCVM.qcow2"
  qemu-img create -f qcow2 "$RCDISK" 64M >/dev/null 2>&1
  if check_then "vm create cloud-hypervisor (disk resize)" ok \
       "$BIN" vm create "$RCVM" --disk "$RCDISK" --backend cloud-hypervisor --memory 256M; then
    # A pré-condição «o vmm está mesmo vivo», verificada e não presumida —
    # a mesma lição da secção CH de snapshots, acima.
    for _ in $(seq 50); do
      pgrep -f -- "^(\S*/)?cloud-hypervisor .*--api-socket $SROOT/vms/$RCVM.sock( |\$)" >/dev/null && break
      sleep 0.2
    done
    check "CH: --disk-size com a VM a correr recusa (5)" 5 "$BIN" vm resize "$RCVM" --disk-size 1
    check "CH: vm stop (disk resize)" ok "$BIN" vm stop "$RCVM"
    check "CH: --disk-size 0 recusa (1)" 1 "$BIN" vm resize "$RCVM" --disk-size 0
    check "CH: --disk-size de uma VM parada" ok "$BIN" vm resize "$RCVM" --disk-size 1
    check "CH: o overlay cresceu para exactamente 1 GiB" ok bash -c \
      "qemu-img info -U --output=json '$SROOT/vms/$RCVM.qcow2' | python3 -c \"import json,sys; sys.exit(0 if json.load(sys.stdin)['virtual-size']==1073741824 else 1)\""
    check "CH: um --disk-size que não cresce recusa (1)" 1 "$BIN" vm resize "$RCVM" --disk-size 1
    "$BIN" delete vm "$RCVM" -f >/dev/null 2>&1
  else
    "$BIN" delete vm "$RCVM" -f >/dev/null 2>&1
  fi
  rm -f "$RCDISK"
else
  skip "vm: --disk-size (cloud-hypervisor)" "$(ch_missing)"
fi

# --- CH: um boot que falha depois de o VMM arrancar não deixa nada em vms/ ----
# Medido depois do #906: um `--firmware` inválido mata o VMM («Invalid bzImage»)
# e ficavam `<nome>.log/.pid/.sock/.sock.lock`; sem registo, o `vm rm` dava
# DX-4501 e não os tirava. Só conta quando o create CHEGOU ao VMM: uma falha
# anterior (infra de rede) não escreve nada e passaria sem medir.
if [[ -n "${DELONIX_ROOT:-}" && -z "${DELONIX_NET_RUNTIME_DIR:-}" ]]; then
  skip "CH: boot falhado não deixa ficheiros em vms/" \
    "DELONIX_ROOT isolado sem DELONIX_NET_RUNTIME_DIR — isola os DOIS ou nenhum"
elif command -v cloud-hypervisor >/dev/null; then
  FVM="chfail-$PFX"; FDIR="$OUT/$FVM"; mkdir -p "$FDIR"
  qemu-img create -f qcow2 "$FDIR/d.qcow2" 64M >/dev/null 2>&1
  echo "not a firmware" > "$FDIR/fw.bin"
  fout=$("$BIN" vm create "$FVM" --disk "$FDIR/d.qcow2" --backend cloud-hypervisor \
    --firmware "$FDIR/fw.bin" --memory 128M 2>&1); frc=$?
  if [[ $frc -ne 0 && "$fout" == *"exited during startup"* ]]; then
    check "CH: boot falhado não deixa ficheiros em vms/" ok bash -c "
      left=\$(ls -A '$SROOT/vms' 2>/dev/null | grep -E '^$FVM(\$|\\.)')
      [ -z \"\$left\" ] || { echo \"ficou: \$left\"; exit 1; }"
  else
    skip "CH: boot falhado não deixa ficheiros em vms/" "o create não chegou ao VMM (rc=$frc)"
  fi
  "$BIN" delete vm "$FVM" -f >/dev/null 2>&1
  rm -rf "$FDIR"
else
  skip "CH: boot falhado não deixa ficheiros em vms/" "sem cloud-hypervisor instalado"
fi

########################################
section "vm: a rede libvirt de uma rede declarada tem a sub-rede declarada, e sai com a última VM"
########################################
# Medido a 2026-10-21 (cli-tester, root isolado, 2× por `vm create` e por
# `stack apply`): desde o #866 o `vm create --network <rede declarada>` define
# uma rede NAT libvirt com o mesmo nome, e (1) nada a removia — nem `vm rm`,
# nem `network rm`, nem `stack destroy`: ficavam a rede, o virbrN e a rota; (2)
# com essa rede deixada para trás, redeclarar a rede noutra sub-rede e criar
# uma VM nela dava rc 0 com a VM na sub-rede ANTIGA. Agora a rede leva a marca
# de posse do motor, só se usa na sub-rede declarada, redefine-se só se é nossa
# e nenhum domínio está nela, e sai com o último registo que a nomeia. Uma rede
# sem a marca nunca se toca. As redes e o domínio «alheios» abaixo são
# definidos por esta secção (sem `net-start`: nem bridge nem rota no host).
if libvirt_system_usable; then
  LNET="lvn-$PFX"; LSTALE="lvs-$PFX"; LFOR="lvf-$PFX"; LDOM="lvd-$PFX"
  LV1="lvn1-$PFX"; LV2="lvn2-$PFX"; LV3="lvn3-$PFX"; LV4="lvn4-$PFX"; LV5="lvn5-$PFX"
  LMARK='delonix.io/xmlns/libvirt/network/1'
  for v in "$LV1" "$LV2" "$LV3" "$LV4" "$LV5"; do qemu-img create -f qcow2 "$OUT/$v.qcow2" 64M >/dev/null 2>&1; done
  lv_net_has() { virsh -c qemu:///system net-dumpxml "$1" 2>/dev/null | grep -q "$2"; }
  lv_net_gone() { ! virsh -c qemu:///system net-info "$1" >/dev/null 2>&1; }
  lv_create() { "$BIN" vm create "$1" --disk "$OUT/$1.qcow2" --backend libvirt --memory 256M --network "$2"; }
  # Uma rede libvirt definida à mão; `$3` = a marca do motor ou nada.
  lv_define_net() {
    printf "<network><name>%s</name>%s<forward mode='nat'/><ip address='%s' netmask='255.255.255.0'/></network>\n" \
      "$1" "$3" "$2" >"$OUT/$1.xml"
    virsh -c qemu:///system net-define "$OUT/$1.xml" >/dev/null 2>&1
  }
  lv_undefine_net() {
    virsh -c qemu:///system net-destroy "$1" >/dev/null 2>&1
    virsh -c qemu:///system net-undefine "$1" >/dev/null 2>&1
  }

  # 1. O caminho do motor: define com a marca, partilha, e sai com o ÚLTIMO
  #    registo — uma VM parada (o `stop` faz undefine do domínio) ainda conta,
  #    e uma posta na rede por `--bridge` (registo `network: ingress`) também:
  #    o cli-tester mediu (2026-10-21, BUG-1) o `rm` da outra a tirar a rede e o
  #    `start` desta a falhar com «Network not found».
  "$BIN" network create "$LNET" --subnet 10.231.91.0/24 >/dev/null 2>&1
  if check_then "vm create libvirt numa rede declarada (posse)" ok lv_create "$LV1" "$LNET"; then
    check "a rede libvirt que o motor define leva a marca de posse e a sub-rede declarada" ok bash -c \
      "$(declare -f lv_net_has); lv_net_has '$LNET' '$LMARK' && lv_net_has '$LNET' \"address='10.231.91.1'\""
    check "uma segunda VM na mesma rede declarada" ok lv_create "$LV2" "$LNET"
    check "uma terceira VM na mesma rede, por --bridge" ok bash -c \
      "'$BIN' vm create '$LV5' --disk '$OUT/$LV5.qcow2' --backend libvirt --memory 256M --net-mode nat --bridge '$LNET' >/dev/null && virsh -c qemu:///system domiflist '$LV5' | grep -q ' $LNET '"
    check "vm stop (posse)" ok "$BIN" vm stop "$LV1"
    check "vm stop (posse, --bridge)" ok "$BIN" vm stop "$LV5"
    check "vm rm com outra VM (parada) na mesma rede deixa a rede libvirt" ok bash -c \
      "'$BIN' vm rm '$LV2' >/dev/null && virsh -c qemu:///system net-info '$LNET' >/dev/null"
    check "vm rm com uma VM (parada) na rede por --bridge deixa a rede libvirt" ok bash -c \
      "'$BIN' vm rm '$LV1' >/dev/null && virsh -c qemu:///system net-info '$LNET' >/dev/null"
    check "vm start da VM por --bridge depois do rm da outra" ok bash -c \
      "'$BIN' vm start '$LV5' >/dev/null && virsh -c qemu:///system domiflist '$LV5' | grep -q ' $LNET '"
    # A rede desapareceu entretanto (o último `rm` de OUTRO root a tirou, ou à
    # mão): o `start` resolve a sub-rede declarada e define-a outra vez.
    "$BIN" vm stop "$LV5" >/dev/null 2>&1
    lv_undefine_net "$LNET"
    check "vm start define outra vez a rede que desapareceu, com a marca e a sub-rede declarada" ok bash -c \
      "$(declare -f lv_net_has); '$BIN' vm start '$LV5' >/dev/null && lv_net_has '$LNET' '$LMARK' && lv_net_has '$LNET' \"address='10.231.91.1'\""
    check "vm rm do último utilizador tira a rede libvirt que o motor definiu" ok bash -c \
      "$(declare -f lv_net_gone); '$BIN' vm rm '$LV5' >/dev/null && lv_net_gone '$LNET'"
  fi
  for v in "$LV1" "$LV2" "$LV5"; do "$BIN" vm rm "$v" -f >/dev/null 2>&1; done
  e2e_net_rm "$LNET"
  lv_net_has "$LNET" "$LMARK" && lv_undefine_net "$LNET"

  # 2. Um resto NOSSO noutra sub-rede (um `vm rm -f` cuja libertação falhou):
  #    com um domínio nele — aqui um do operador — recusa (5) e nomeia-o; sem
  #    domínios, redefine-o na sub-rede declarada.
  if lv_define_net "$LSTALE" 10.231.95.1 "<metadata><d:network xmlns:d='https://$LMARK' subnet='10.231.95.0/24'/></metadata>"; then
    printf "<domain type='qemu'><name>%s</name><memory unit='MiB'>64</memory><os><type>hvm</type></os><devices><interface type='network'><source network='%s'/></interface></devices></domain>\n" \
      "$LDOM" "$LSTALE" >"$OUT/$LDOM.xml"
    virsh -c qemu:///system define "$OUT/$LDOM.xml" >/dev/null 2>&1
    "$BIN" network create "$LSTALE" --subnet 10.231.96.0/24 >/dev/null 2>&1
    check "rede nossa noutra sub-rede com um domínio nela: o vm create recusa (5) e nomeia-o" ok bash -c \
      "out=\$($(declare -f lv_create); BIN='$BIN' OUT='$OUT' lv_create '$LV3' '$LSTALE' 2>&1); rc=\$?; [ \$rc -eq 5 ] && printf '%s' \"\$out\" | grep -q '$LDOM'"
    check "a recusa não definiu o domínio da VM nem mexeu na rede" ok bash -c \
      "$(declare -f lv_net_has); ! virsh -c qemu:///system domstate '$LV3' >/dev/null 2>&1 && lv_net_has '$LSTALE' \"address='10.231.95.1'\""
    virsh -c qemu:///system undefine "$LDOM" >/dev/null 2>&1
    "$BIN" vm rm "$LV3" -f >/dev/null 2>&1
    check "rede nossa noutra sub-rede sem domínios: o vm create redefine-a na declarada" ok bash -c \
      "$(declare -f lv_net_has lv_create); BIN='$BIN' OUT='$OUT' lv_create '$LV3' '$LSTALE' >/dev/null && lv_net_has '$LSTALE' \"address='10.231.96.1'\" && lv_net_has '$LSTALE' '$LMARK'"
    check "vm rm leva a rede redefinida com a VM" ok bash -c \
      "$(declare -f lv_net_gone); '$BIN' vm rm '$LV3' >/dev/null && lv_net_gone '$LSTALE'"
  else
    skip "rede nossa noutra sub-rede" "o virsh net-define da rede de teste falhou neste host"
  fi
  "$BIN" vm rm "$LV3" -f >/dev/null 2>&1
  virsh -c qemu:///system undefine "$LDOM" >/dev/null 2>&1
  e2e_net_rm "$LSTALE"
  lv_undefine_net "$LSTALE"

  # 3. Uma rede SEM a marca (do operador, ou deixada por um binário anterior a
  #    esta correcção) noutra sub-rede: recusa (5), e fica como estava.
  if lv_define_net "$LFOR" 10.231.93.1 ""; then
    "$BIN" network create "$LFOR" --subnet 10.231.94.0/24 >/dev/null 2>&1
    check "rede libvirt do operador noutra sub-rede: o vm create recusa (5) e diz que não é nossa" ok bash -c \
      "out=\$($(declare -f lv_create); BIN='$BIN' OUT='$OUT' lv_create '$LV4' '$LFOR' 2>&1); rc=\$?; [ \$rc -eq 5 ] && printf '%s' \"\$out\" | grep -q 'did not define'"
    check "a rede do operador continua lá, parada e sem a marca" ok bash -c \
      "virsh -c qemu:///system net-dumpxml '$LFOR' | grep -q 10.231.93.1 && ! virsh -c qemu:///system net-dumpxml '$LFOR' | grep -q '$LMARK' && virsh -c qemu:///system net-info '$LFOR' | grep -q 'Active: *no'"
  else
    skip "rede libvirt do operador com o mesmo nome" "o virsh net-define da rede de teste falhou neste host"
  fi
  "$BIN" vm rm "$LV4" -f >/dev/null 2>&1
  e2e_net_rm "$LFOR"
  lv_undefine_net "$LFOR"
  for v in "$LV1" "$LV2" "$LV3" "$LV4" "$LV5" "$LSTALE" "$LFOR" "$LDOM"; do
    rm -f "$OUT/$v.qcow2" "$OUT/$v.xml"
  done
else
  skip "vm: rede libvirt de uma rede declarada" "sem virsh/qemu-img/qemu-system-x86_64, ou sem ligação libvirt de sistema"
fi

########################################
section "vm: o mesmo nome noutra namespace é um conflito, não «ensured»"
########################################
# ADR-0069 item 2, medido a 2026-10-06: o registo de uma VM é `vms/<nome>.json`,
# qualquer que seja a namespace que declare, por isso o nome é ÚNICO no nó — e
# um manifesto que peça o mesmo nome noutra namespace estava a receber
# `stack plan` = «1 unchanged» e `stack apply` = «vm/<nome>: ensured» com rc=0,
# com a VM a ficar na PRIMEIRA namespace. Numa VM a namespace É a fronteira de
# isolamento (`@dlxns_<ns>`), logo o inquilino que pediu a segunda ficava com uma
# VM que a primeira alcança e a dele não. É a mesma regra que um Pod já diz, e
# aqui a consequência é de isolamento, não cosmética.
#
# Precisa do Cloud Hypervisor: o libvirt RECUSA uma VM com namespace (DX-1508,
# as VMs dele vivem fora da SDN), por isso a pré-condição — um registo em
# `teamA` — só se consegue aqui.
if [[ -n "${DELONIX_ROOT:-}" && -z "${DELONIX_NET_RUNTIME_DIR:-}" ]]; then
  skip "vm: nome repetido noutra namespace" \
    "DELONIX_ROOT isolado sem DELONIX_NET_RUNTIME_DIR — isola os DOIS ou nenhum"
elif ch_usable; then
  NSW="$WORK/vm-namespace"; mkdir -p "$NSW"; NSVM="nsvm-$PFX"; NSNET="nsnet-$PFX"
  qemu-img create -f qcow2 "$NSW/d.qcow2" 64M >/dev/null 2>&1
  check "vm namespace: a rede da VM cria-se" ok "$BIN" network create "$NSNET"
  for ns in teamA teamB; do
    cat >"$NSW/$ns.yaml" <<YAML
apiVersion: compute.delonix.io/v1alpha1
kind: VirtualMachine
metadata:
  name: $NSVM
  namespace: $ns
spec:
  disk: $NSW/d.qcow2
  memory: 128M
  vcpus: 1
  network: $NSNET
  provider:
    type: cloud-hypervisor
YAML
  done
  if check_then "vm namespace: stack apply da VM CH em teamA" ok "$BIN" stack apply -f "$NSW/teamA.yaml"; then
    check "o mesmo nome noutra namespace é um conflito (5)" 5 \
      "$BIN" stack apply -f "$NSW/teamB.yaml"
    check "... e a recusa nomeia as duas namespaces" ok bash -c "
      '$BIN' stack apply -f '$NSW/teamB.yaml' 2>&1 | grep -q \"already exists in namespace 'teamA'\""
    check "... e a VM continua na namespace da primeira" ok bash -c "
      '$BIN' vm ls --namespace teamA | grep -q '$NSVM'"
  fi
  "$BIN" delete vm "$NSVM" -f >/dev/null 2>&1
  e2e_net_rm "$NSNET"
  rm -rf "$NSW"
else
  skip "vm: nome repetido noutra namespace" "$(ch_missing)"
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
# `$IMG`, nunca um segundo nome: isto dizia `alpine:latest`, que o guarda de
# imagem do topo não cobre, e por isso era um SEGUNDO pull. Quando ele falha (a
# ligação de saída deste host é lenta), o container não existe e os treze checks
# abaixo chumbam a dizer «no such container» — treze passos depois do sítio onde
# o problema está. Medido a 2026-10-06, numa corrida em que a imagem só chegou
# ao store depois desta secção.
#
# E é um CHECK, não um comando mudo: um `run` que falha com a saída descartada
# faz uma falha do AMBIENTE ler-se como treze falhas do motor.
check "o container do backup arrancou (pré-condição da secção)" ok \
  "$BIN" container run -d --name "$BKC" -v "$BKV":/data "$IMG" sleep 300
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

e2e_rm "$BKC"
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
# Como no da capability acima, e pela mesma razão: um `fail` sozinho conta
# QUALQUER recusa. Desde o ADR-0075 D1 o `serve cri` tem uma segunda razão para
# falhar — o irmão não estar ao lado do binário — e essa recusa não fala de
# tectos. Sem esta linha o troço ficava verde a medir o engano, que é a quarta
# forma de não testar nada descrita no cabeçalho. Medido: a recusa vem do
# `delonix-cri` e diz `invalid capability ceiling mode "xyz"`.
check "e a recusa NOMEIA o modo" ok bash -c \
  "'$BIN' serve cri --addr 'unix://$CAPSOCK' --cap-ceiling-mode xyz 2>&1 | grep -q xyz"
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

# --- ADR-0074 D1: um hostPort que o nó não publica é RECUSADO pelo nome -------
# A nota acima diz que não se finge um pedido gRPC aqui. Com o `crictl` presente
# — o cliente oficial do projecto Kubernetes — já não é fingir: é o pedido real,
# pelo mesmo transporte que o kubelet usa.
#
# Medido a 2026-10-07: o `slirp4netns` RECUSA SCTP no `add_hostfwd` («bad
# arguments.proto»), e o `portmap` do CNI fala tcp/udp. Antes desta recusa o
# sandbox era criado, a spec `5070:5070/sctp` chegava ao container e o pod morria
# no `StartContainer` a nomear a SPEC e não a causa, em ciclo. O veredicto aqui
# são as DUAS metades: a recusa chega, e não fica sandbox nenhum para limpar.
if ! command -v crictl >/dev/null 2>&1; then
  skip "CRI: hostPort SCTP recusado" "o crictl não está instalado neste host"
else
  CRISOCK2="/tmp/dlx-cri-sctp-$PFX.sock"
  CRIPID2="$(e2e_serve_up cri "$CRISOCK2")"
  if ! cri_serving "CRI hostPort SCTP" "$CRIPID2" "$CRISOCK2"; then
    :  # já é um FAIL, com o log do servidor
  else
    CRICFG="$OUT/pod-sctp.json"
    cat > "$CRICFG" <<'JSON'
{
  "metadata": { "name": "sip", "uid": "sctp-uid-1", "namespace": "default", "attempt": 0 },
  "port_mappings": [ { "protocol": 2, "container_port": 5070, "host_port": 5070 } ],
  "linux": {}
}
JSON
    # A recusa chega pelo transporte, e NOMEIA a porta e o transporte — um
    # «unsupported» seco deixaria quem lê sem saber o que mudar.
    check "CRI: um hostPort SCTP é recusado pelo crictl" fail \
      crictl --runtime-endpoint "unix://$CRISOCK2" runp "$CRICFG"
    check "CRI: a recusa do SCTP nomeia a porta e o transporte" ok bash -c \
      "crictl --runtime-endpoint 'unix://$CRISOCK2' runp '$CRICFG' 2>&1 | grep -q 5070 && \
       crictl --runtime-endpoint 'unix://$CRISOCK2' runp '$CRICFG' 2>&1 | grep -qi sctp"
    # A metade que diz que a recusa foi ANTES de criar: o nó não tem sandbox.
    check "CRI: um sandbox recusado não fica para trás" ok bash -c \
      "[ \"\$(crictl --runtime-endpoint 'unix://$CRISOCK2' pods -q 2>/dev/null | grep -c .)\" = 0 ]"
    # E o controlo: um hostPort TCP, no mesmo nó e pelo mesmo caminho, passa —
    # senão este troço ficaria verde com a recusa a ser de tudo.
    CRICFG_TCP="$OUT/pod-tcp.json"
    cat > "$CRICFG_TCP" <<'JSON'
{
  "metadata": { "name": "web", "uid": "tcp-uid-1", "namespace": "default", "attempt": 0 },
  "port_mappings": [ { "protocol": 0, "container_port": 80, "host_port": 51091 } ],
  "linux": {}
}
JSON
    CRIPOD="$(crictl --runtime-endpoint "unix://$CRISOCK2" runp "$CRICFG_TCP" 2>/dev/null || true)"
    check "CRI: um hostPort TCP continua a ser aceite (controlo)" ok bash -c \
      "[ -n '$CRIPOD' ]"
    [ -n "$CRIPOD" ] && crictl --runtime-endpoint "unix://$CRISOCK2" rmp -f "$CRIPOD" >/dev/null 2>&1
  fi
  [ -n "${CRIPID2:-}" ] && kill "$CRIPID2" 2>/dev/null
  for i in $(seq 1 40); do kill -0 "${CRIPID2:-0}" 2>/dev/null || break; sleep 0.2; done
  rm -f "$CRISOCK2"
fi

# --- ADR-0038 item 3: cpuset_mems/unified/hugepage_limits, honour-or-refuse ---
# Até esta mudança o CRI não tinha preflight NENHUM para um campo de recurso —
# nem para o `cpuset_cpus` já ligado desde sempre: um kubelet a pedir um
# controlador que a leaf do container não tem recebia um container `Running`
# com o limite simplesmente ausente, a mesma falha que o #307 já fechou para
# `-m`/`--cpus`/`--cpuset` na CLI. A recusa corre em `StartContainer`, não em
# `CreateContainer` — por isso o `create` abaixo tem de ter SUCESSO, e é o
# `start` a seguir que recusa.
#
# O controlador "nonexistentctrl" nunca existe em kernel nenhum, por isso a
# recusa é determinística em QUALQUER host — nem precisa de root nem de saber
# o que este host delega. A válvula `DELONIX_ALLOW_UNENFORCED_LIMITS` é o
# controlo: o MESMO pedido, com ela no ambiente do servidor, tem de arrancar.
# E o traçado de caminho (`hugepage_limits[].page_size`/chave `unified` com
# `..`) é recusado SEMPRE, incluindo com a válvula aberta — ela só dispensa a
# pergunta ao controlador, nunca a segurança do caminho do ficheiro.
if ! command -v crictl >/dev/null 2>&1; then
  skip "CRI: cpuset_mems/unified/hugepage_limits honour-or-refuse" "o crictl não está instalado neste host"
else
  CRIRES_POD="$OUT/pod-res.json"
  cat > "$CRIRES_POD" <<'JSON'
{ "metadata": { "name": "respod", "uid": "resc-uid-1", "namespace": "default", "attempt": 0 }, "linux": {} }
JSON
  CRIRES_BOGUS="$OUT/ctr-res-bogus.json"
  cat > "$CRIRES_BOGUS" <<JSON
{
  "metadata": { "name": "resc", "attempt": 0 },
  "image": { "image": "$IMG" },
  "command": ["sleep", "300"],
  "linux": { "resources": { "unified": { "nonexistentctrl.limit": "1" } } }
}
JSON
  CRIRES_ESCAPE="$OUT/ctr-res-escape.json"
  cat > "$CRIRES_ESCAPE" <<JSON
{
  "metadata": { "name": "resc2", "attempt": 0 },
  "image": { "image": "$IMG" },
  "command": ["sleep", "300"],
  "linux": { "resources": { "hugepage_limits": [ { "page_size": "../etc/passwd", "limit": 1 } ] } }
}
JSON

  CRISOCK3="/tmp/dlx-cri-res-$PFX.sock"
  CRIPID3="$(e2e_serve_up cri "$CRISOCK3")"
  if ! cri_serving "CRI honour-or-refuse" "$CRIPID3" "$CRISOCK3"; then
    :  # já é um FAIL, com o log do servidor
  else
    C3="crictl --runtime-endpoint unix://$CRISOCK3 --image-endpoint unix://$CRISOCK3"
    POD3="$($C3 runp "$CRIRES_POD" 2>/dev/null)"
    CID3="$($C3 create "$POD3" "$CRIRES_BOGUS" "$CRIRES_POD" 2>/dev/null)"
    check "CRI: create com um recurso que o leaf não delega tem sucesso" ok bash -c \
      "[ -n '$CID3' ]"
    # UM SÓ `start`, capturado — chamá-lo duas vezes (uma por grep) testaria o
    # SEGUNDO pedido num container já falhado a arrancar, que não é o mesmo
    # caso: a 1.ª versão deste check fazia isso e o 2.º `start` respondia outra
    # coisa, FAIL por um bug do próprio check, não do motor.
    CRIRES_OUT3="$(crictl --runtime-endpoint "unix://$CRISOCK3" start "$CID3" 2>&1)"
    CRIRES_RC3=$?
    check "CRI: start recusa um controlador inexistente" fail bash -c "exit $CRIRES_RC3"
    # Sem o traçado exacto de `[`/`\"` — o klog do crictl escreve o campo com
    # barras invertidas antes das aspas (`unified[\"...\"]`), e um regex que
    # tentasse casar essa forma à letra discutia escape em vez de medir o que
    # importa: que a mensagem nomeia os DOIS, o campo e o controlador.
    check "CRI: a recusa nomeia o campo e o controlador" ok bash -c \
      "printf '%s' '$CRIRES_OUT3' | grep -q unified && \
       printf '%s' '$CRIRES_OUT3' | grep -q nonexistentctrl"
    [ -n "$CID3" ] && crictl --runtime-endpoint "unix://$CRISOCK3" rm -f "$CID3" >/dev/null 2>&1
    [ -n "$POD3" ] && crictl --runtime-endpoint "unix://$CRISOCK3" rmp -f "$POD3" >/dev/null 2>&1

    # Um `hugepage_limits[].page_size`/chave `unified` com `..` é recusado no
    # `start`, ANTES de perguntar a controlador nenhum — e continua a ser
    # mesmo depois de reportado o controlador (ver abaixo).
    POD3b="$($C3 runp "$CRIRES_POD" 2>/dev/null)"
    CID3b="$($C3 create "$POD3b" "$CRIRES_ESCAPE" "$CRIRES_POD" 2>/dev/null)"
    CRIRES_OUT3B="$(crictl --runtime-endpoint "unix://$CRISOCK3" start "$CID3b" 2>&1)"
    CRIRES_RC3B=$?
    check "CRI: um page_size com .. é recusado por nome, nunca escrito" fail bash -c "exit $CRIRES_RC3B"
    check "CRI: a recusa do page_size é por traçado de caminho, não por controlador" ok bash -c \
      "printf '%s' '$CRIRES_OUT3B' | grep -q 'invalid page size'"
    [ -n "$CID3b" ] && crictl --runtime-endpoint "unix://$CRISOCK3" rm -f "$CID3b" >/dev/null 2>&1
    [ -n "$POD3b" ] && crictl --runtime-endpoint "unix://$CRISOCK3" rmp -f "$POD3b" >/dev/null 2>&1
  fi
  [ -n "${CRIPID3:-}" ] && kill "$CRIPID3" 2>/dev/null
  for i in $(seq 1 40); do kill -0 "${CRIPID3:-0}" 2>/dev/null || break; sleep 0.2; done
  rm -f "$CRISOCK3"

  # O controlo: a MESMA recusa, com a válvula no ambiente do SERVIDOR (não do
  # crictl) — tem de arrancar. Prova que `DELONIX_ALLOW_UNENFORCED_LIMITS`
  # chega a sério ao caminho novo, não só ao da CLI que já a tinha.
  CRISOCK4="/tmp/dlx-cri-res-hatch-$PFX.sock"
  rm -f "$CRISOCK4"
  setsid env DELONIX_ALLOW_UNENFORCED_LIMITS=1 "$BIN" serve cri --addr "unix://$CRISOCK4" >>"$SRVLOG" 2>&1 &
  for i in $(seq 1 60); do [ -S "$CRISOCK4" ] && break; sleep 0.2; done
  CRIPID4="$(ss -xlpnH 2>/dev/null | grep -F "$CRISOCK4" | grep -o 'pid=[0-9]*' | head -1 | cut -d= -f2)"
  if ! cri_serving "CRI com DELONIX_ALLOW_UNENFORCED_LIMITS" "$CRIPID4" "$CRISOCK4"; then
    :  # já é um FAIL, com o log do servidor
  else
    C4="crictl --runtime-endpoint unix://$CRISOCK4 --image-endpoint unix://$CRISOCK4"
    POD4="$($C4 runp "$CRIRES_POD" 2>/dev/null)"
    CID4="$($C4 create "$POD4" "$CRIRES_BOGUS" "$CRIRES_POD" 2>/dev/null)"
    check "CRI: a válvula deixa arrancar o MESMO pedido que foi recusado sem ela" ok \
      crictl --runtime-endpoint "unix://$CRISOCK4" start "$CID4"
    [ -n "$CID4" ] && crictl --runtime-endpoint "unix://$CRISOCK4" stop "$CID4" >/dev/null 2>&1
    [ -n "$CID4" ] && crictl --runtime-endpoint "unix://$CRISOCK4" rm -f "$CID4" >/dev/null 2>&1
    [ -n "$POD4" ] && crictl --runtime-endpoint "unix://$CRISOCK4" rmp -f "$POD4" >/dev/null 2>&1
  fi
  [ -n "${CRIPID4:-}" ] && kill "$CRIPID4" 2>/dev/null
  for i in $(seq 1 40); do kill -0 "${CRIPID4:-0}" 2>/dev/null || break; sleep 0.2; done
  rm -f "$CRISOCK4"
fi

# --- ADR-0074 D3: o hostPort em modo root/CNI é PUBLICADO ----------------------
# Até esta mudança o caminho CNI não publicava `hostPort` para protocolo NENHUM:
# os mapeamentos eram guardados e depois descartados (a guarda do `run_opts_of`
# salta um sandbox CNI), logo o pod subia `Running` com uma porta que não
# respondia. Silêncio, rc=0, para tcp, udp e sctp.
#
# Medido a 2026-10-07 numa VM da golden `delonix-vm-k8s:1.36` (que já traz o
# `portmap`), com o MESMO guião contra os dois binários: a `origin/main` criou o
# sandbox com 0 regras DNAT e a porta muda; com a correcção, 3 regras e `LAB-OK`
# pela porta do host, e 0 regras depois do `rmp`.
#
# Este troço precisa de root E de uma cadeia CNI que DECLARE `portMappings` — a
# bateria corre rootless, por isso salta aqui com a razão em vez de ficar verde
# por ausência. Para o correr: VM da golden, `/etc/cni/net.d` com uma conflist
# `bridge` + `portmap`, e `sudo`.
if [ "$(id -u)" != 0 ]; then
  skip "CRI root/CNI: o hostPort é publicado" "exige root e uma conflist CNI com \`portmap\` (corre numa VM da golden delonix-vm-k8s)"
elif ! command -v crictl >/dev/null 2>&1; then
  skip "CRI root/CNI: o hostPort é publicado" "o crictl não está instalado"
elif ! ls /etc/cni/net.d/*.conflist >/dev/null 2>&1 || ! grep -ql portMappings /etc/cni/net.d/*.conflist 2>/dev/null; then
  skip "CRI root/CNI: o hostPort é publicado" "nenhuma conflist em /etc/cni/net.d declara a capacidade portMappings"
elif [ ! -x /opt/cni/bin/portmap ]; then
  skip "CRI root/CNI: o hostPort é publicado" "o plugin portmap não está em /opt/cni/bin"
else
  CRISOCK3="/run/dlx-cri-hp-$PFX.sock"
  CRIPID3="$(e2e_serve_up cri "$CRISOCK3")"
  if ! cri_serving "CRI root/CNI hostPort" "$CRIPID3" "$CRISOCK3"; then
    :  # já é um FAIL, com o log do servidor
  else
    C3="crictl --runtime-endpoint unix://$CRISOCK3"
    HPCFG="$OUT/pod-hostport.json"
    cat > "$HPCFG" <<'JSON'
{
  "metadata": { "name": "hp", "uid": "hp-uid-1", "namespace": "default", "attempt": 0 },
  "port_mappings": [ { "protocol": 0, "container_port": 80, "host_port": 31080 } ],
  "linux": {}
}
JSON
    HPPOD="$($C3 runp "$HPCFG" 2>/dev/null | tail -1)"
    check "CRI root/CNI: o sandbox com hostPort é criado" ok bash -c "[ -n '$HPPOD' ]"
    # O veredicto são as REGRAS no kernel, não o rc do runp — a base também
    # devolvia 0 e criava o sandbox, e era isso que escondia o defeito.
    check "CRI root/CNI: o portmap escreveu o DNAT da porta" ok bash -c \
      "[ \"\$(iptables -t nat -S | grep -c 'dport 31080')\" -gt 0 ]"
    # E a prova que vale: a porta RESPONDE de dentro da netns do sandbox.
    ip netns exec "cri-$HPPOD" sh -c \
      'printf "HTTP/1.1 200 OK\r\nContent-Length: 7\r\n\r\nLAB-OK\n" | nc -l -p 80 -q 2 >/dev/null 2>&1 &' 2>/dev/null
    sleep 1
    check "CRI root/CNI: o hostPort RESPONDE" ok bash -c \
      "[ \"\$(curl -s --max-time 5 http://127.0.0.1:31080/ 2>/dev/null)\" = LAB-OK ]"
    $C3 rmp -f "$HPPOD" >/dev/null 2>&1
    # O DEL tem de desfazer: a conflist guardada leva o runtimeConfig, que é o
    # que o portmap precisa de receber de volta.
    check "CRI root/CNI: o rmp limpa as regras" ok bash -c \
      "[ \"\$(iptables -t nat -S | grep -c 'dport 31080')\" = 0 ]"
    # A recusa de uma cadeia que NÃO declara a capacidade fica no teste
    # unitário (`a_cni_chain_that_cannot_publish_is_refused_with_the_fix`): o
    # directório da conflist é uma constante (`cni::DEFAULT_CONF_DIR`) e não uma
    # variável, logo não há como apontar o servidor VIVO para outra cadeia sem
    # reescrever o /etc do nó — e um check que medisse outra coisa seria pior
    # que a sua ausência.
  fi
  [ -n "${CRIPID3:-}" ] && kill "$CRIPID3" 2>/dev/null
  for i in $(seq 1 40); do kill -0 "${CRIPID3:-0}" 2>/dev/null || break; sleep 0.2; done
  rm -f "$CRISOCK3"
fi

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

# `delonix serve node-api` executa o `delonix-node-api` (ADR-0040 P5), o
# mesmo contrato das três acima — foreground, exit real, nunca só
# backgrounded (os dois usos de `serve node-api` noutras secções nunca
# julgam o comando em si, só a sonda ao socket que ele acaba por abrir).
NODEAPISOCK2="/tmp/dlx-srv-nodeapi2-$PFX.sock"
NODEAPIPID2="$(e2e_serve_up node-api "$NODEAPISOCK2")"
check "serve node-api corre o executável delonix-node-api" ok bash -c \
  "[ -n '$NODEAPIPID2' ] && [ \"\$(basename \"\$(readlink /proc/$NODEAPIPID2/exe)\")\" = delonix-node-api ]"
[ -n "$NODEAPIPID2" ] && kill "$NODEAPIPID2" 2>/dev/null
for i in $(seq 1 40); do kill -0 "$NODEAPIPID2" 2>/dev/null || break; sleep 0.2; done
rm -f "$NODEAPISOCK2"
LONE="$OUT/lone-nodeapi-$PFX"; mkdir -p "$LONE"; cp "$BIN" "$LONE/delonix"
check "serve node-api sem o delonix-node-api instalado sai com 69" 69 \
  env PATH=/usr/bin:/bin "$LONE/delonix" serve node-api --addr "unix:///tmp/dlx-lone-nodeapi-$PFX.sock"
rm -rf "$LONE"

check "nenhum servidor desta corrida ficou para trás" ok bash -c \
  "! pgrep -f '(serve (cri|api|node-api|docker-api)|delonix-cri|delonix-mgmt|delonix-node-api) --addr unix:///tmp/dlx-srv-.*$PFX' >/dev/null"

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

  # --- deny/rm/unpublish: os irmãos de allow/ls/clear que nunca corriam -------
  NGNET="ngnet-$PFX"; NGC="ngc-$PFX"; e2e_port NGP
  if check_then "net ingress/egress: a rede cria-se" ok "$BIN" network create "$NGNET" \
     && check_then "net ingress/egress: o container com -p arranca" ok \
          "$BIN" container run -d --name "$NGC" --net "$NGNET" -p "$NGP:80" "$IMG" sleep 600; then
    check "net ingress deny" ok "$BIN" net ingress deny "$NGC" 80
    check "net ingress deny: a regra aparece na firewall do container" ok bash -c \
      "'$BIN' net ingress ls '$NGC' | grep -qE '^any +80 .*deny'"
    check "net ingress rm" ok "$BIN" net ingress rm "$NGC" 80
    check "net ingress rm: a regra saiu" ok bash -c \
      "! '$BIN' net ingress ls '$NGC' | grep -qE '^any +80 .*deny'"
    check "net ingress unpublish" ok "$BIN" net ingress unpublish "$NGC" "$NGP"
    check "net ingress unpublish: a porta saiu da listagem" ok bash -c \
      "'$BIN' net ingress ls -o json | python3 -c \"import json,sys; d=json.load(sys.stdin); sys.exit(1 if any('$NGP' in t for r in d for t in r.get('targets',[])) else 0)\""
    check "net egress deny" ok "$BIN" net egress deny "$NGC" 80
    check "net egress deny: a regra aparece na firewall do container" ok bash -c \
      "'$BIN' net egress ls '$NGC' | grep -qE '^any +80 .*deny'"
    check "net egress rm" ok "$BIN" net egress rm "$NGC" 80
    check "net egress rm: a regra saiu" ok bash -c \
      "! '$BIN' net egress ls '$NGC' | grep -qE '^any +80 .*deny'"
  fi
  e2e_rm "$NGC"
  e2e_net_rm "$NGNET"

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

  # --- system boot enable / disable: the units, in a HOME of our own ----------
  # These two write systemd units under `$HOME/.config/systemd/user` (boot.rs
  # reads HOME directly), which the DELONIX_ROOT does NOT redirect — which is
  # why they were skipped here for so long. HOME pointed inside the run's own
  # root makes the pair measurable without touching the host: this operator's
  # real `~/.config/systemd/user` holds their own tooling, and a `disable` that
  # deleted a unit it had not written is a defect this repo has already had
  # (hence `claims_unit`, the OWN_MARKER filter). So the checks below assert
  # three things and not the exit code: the unit APPEARS, it is GONE after
  # `disable`, and a foreign unit sitting beside it SURVIVES.
  #
  # `systemctl --user` still talks to the live user manager, which resolves its
  # search path from the real HOME and so will not find these units: the call
  # fails and `enable` writes the files anyway (`fs::write`, unconditional).
  # That is the intended shape here — the files are the contract, not the
  # manager's opinion of them. `loginctl enable-linger` is a no-op on a host
  # that already has `Linger=yes`; it is left as it was found either way.
  BOOTHOME="$DELONIX_ROOT/boot-home"
  BOOTUD="$BOOTHOME/.config/systemd/user"
  REALUD="$HOME/.config/systemd/user"
  mkdir -p "$BOOTUD"
  # A foreign unit named like the one from the real incident. No OWN_MARKER, so
  # `disable` has no claim on it.
  printf '[Unit]\nDescription=not written by delonix\n[Service]\nExecStart=/bin/true\n' \
    >"$BOOTUD/delonix-issues-sync.service"
  # The host's own units, before. The whole SUBTREE, and symlinks with it: the
  # likeliest way this pair reaches outside the isolation is not a unit file,
  # it is `systemctl --user enable` dropping a link into
  # `default.target.wants/`. Name, type and size, so an addition, a deletion,
  # an edit or a retargeted link all show up.
  # Same expression as the check below, which runs in its own shell and so
  # cannot share a helper.
  REAL_BEFORE="$(find "$REALUD" -printf '%p %y %s\n' 2>/dev/null | sort | md5sum)"
  # A container that WANTS to be up, so `enable` has a unit to write at all:
  # the units follow the store, and the section above removed its container.
  # `--net none` on purpose — this leaf has nothing to do with the SDN, and the
  # holder is shared.
  BOOTC="bt-$PFX"
  "$BIN" container run -d --net none --restart always --name "$BOOTC" "$IMG" sleep 120 >/dev/null 2>&1 || true
  check "system boot enable: the unit for a restart=always container is written" ok bash -c "
    HOME='$BOOTHOME' '$BIN' system boot enable >/dev/null 2>&1 || exit 1
    ls '$BOOTUD' | grep -q '$BOOTC' || { echo 'no unit for $BOOTC:'; ls -A '$BOOTUD'; exit 1; }"
  check "system boot disable: the written unit goes, the foreign one stays" ok bash -c "
    HOME='$BOOTHOME' '$BIN' system boot disable >/dev/null 2>&1 || exit 1
    ls '$BOOTUD' | grep -q '$BOOTC' && { echo 'the unit it wrote survived disable'; exit 1; }
    [ -f '$BOOTUD/delonix-issues-sync.service' ] || { echo 'disable DELETED a unit it did not write'; exit 1; }
    grep -q 'not written by delonix' '$BOOTUD/delonix-issues-sync.service' || { echo 'the foreign unit was rewritten'; exit 1; }"
  check "system boot enable/disable left the host's own units alone" ok bash -c "
    now=\$(find '$REALUD' -printf '%p %y %s\n' 2>/dev/null | sort | md5sum)
    [ \"\$now\" = '$REAL_BEFORE' ] || {
      echo 'the host unit dir changed: was $REAL_BEFORE now '\"\$now\"; exit 1; }"
  "$BIN" container rm -f "$BOOTC" >/dev/null 2>&1 || true

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

section "net — ciclo de vida: o que fica no holder depois de um rm"

# Porque esta secção existe (auditoria NaaS, doc 62 §6 P1, sessão S3): `network
# rm` apagava a rede debaixo de um container a correr, e o `netdel` só tirava o
# link — `@dlxbr`, `@netpair`, a regra de egress e a thread DHCP ficavam, e a rede
# recriada com o mesmo nome nascia sem DHCP e com o `deny` da anterior. Um publish
# que falhava a meio deixava a primeira porta a escutar no host sem dono, e a rede
# que uma VM criava não tinha registo (`network ls` não a via, `rm` dizia 4).
# Medido contra `d3d6f394`: 8/8 chumbam. Cada check CONTA o que resta (nft, ss,
# `network ls`, portas do host) em vez de confiar no código de saída do comando.
#
# A lógica vive em `scripts/net-lifecycle-leaks.sh` para correr sozinha contra
# dois binários; aqui é UM check, e o detalhe sai nas linhas do FAIL.
if [[ -z "${DELONIX_ROOT:-}" || -z "${DELONIX_NET_RUNTIME_DIR:-}" ]]; then
  skip "net: ciclo de vida sem fugas" "exige DELONIX_ROOT E DELONIX_NET_RUNTIME_DIR (ver cabeçalho)"
else
  check "net: ciclo de vida sem fugas (rm, DHCP, egress, publish, rede de VM)" ok \
    env E2E_IMAGE="$IMG" bash "$(dirname "$0")/net-lifecycle-leaks.sh" "$BIN" "lk$PFX"
fi

section "providers remotos: NetworkGateway e NetworkZone — posse pela marca, não pelo nome"
########################################
# Auditoria 62, §6 P1 (S6). O que se prova em QUALQUER máquina é a recusa
# honesta sem appliance. O resto — a marca de posse, a recusa de adoptar por
# nome, a recusa com pendentes alheios — só existe contra um OPNsense e um
# cluster Proxmox reais; sem eles cada check sai como SKIP com a razão, que o
# resumo conta como NÃO COBERTO. Nunca PASS por omissão.
RWORK="$OUT/remote-$PFX"; mkdir -p "$RWORK"
GW="gw-$PFX"
ZN="z$(( $$ % 100000 ))"            # id SDN: letra + até 7 minúsculas/dígitos
VN="v$(( $$ % 100000 ))a"

cat > "$RWORK/gw-native.yaml" <<YAML
apiVersion: networking.delonix.io/v1alpha1
kind: NetworkGateway
metadata: { name: $GW-native }
spec:
  provider: native
  aliases: [{ name: dlx_$PFX, kind: host, content: ["10.99.0.1"] }]
YAML
# D1 regra 2 do ADR-0059: nomear um provider que ninguém registou pede OUTRO
# provider, por isso sai como indisponível (69) com o reason da D5, e não 1.
check "NetworkGateway num provider que ninguém registou recusa com 69 (o native saiu no F2b)" 69 \
  "$BIN" apply -f "$RWORK/gw-native.yaml"
check "... e a linha de erro traz o código do reason unsupported_capability (DX-6381)" ok bash -c \
  "'$BIN' apply -f '$RWORK/gw-native.yaml' 2>&1 | grep -q 'error\\[DX-6381\\]'"
# O envelope da D5 na CLI: o reason e o contexto (provider, papel, passo) vão
# por baixo da linha de erro.
check "... com o reason, o provider nomeado e o papel por baixo da linha de erro" ok bash -c \
  "out=\$('$BIN' apply -f '$RWORK/gw-native.yaml' 2>&1); echo \"\$out\" | grep -q 'reason: unsupported_capability' && echo \"\$out\" | grep -q 'provider: native' && echo \"\$out\" | grep -q 'role: gateway'"
# Um número publicado nunca muda de significado: o que a D5 retirou continua a
# ter resposta no explain, com o sucessor.
check "explain de um código retirado (DX-5340) responde e nomeia o sucessor DX-5389" ok bash -c \
  "'$BIN' explain DX-5340 | grep -q 'DX-5389'"
check "explain DX-6381 diz o reason unsupported_capability" ok bash -c \
  "'$BIN' explain DX-6381 | grep -q 'unsupported_capability'"
# O provider é resolvido ANTES de o registo ser gravado: um nome que ninguém
# registou é recusado sem deixar registo, e o delete termina na mesma.
check "delete de um NetworkGateway recusado termina" ok \
  "$BIN" delete networkgateways "$GW-native"
check "... e o registo desaparece" ok bash -c \
  "! '$BIN' get networkgateways 2>/dev/null | grep -q '$GW-native'"
# Um registo que um build anterior ao F2b deixou, a nomear o `native`: esse
# provider recusava todas as escritas, logo nada remoto tem a marca dele, e o
# delete tira o registo sem precisar de provider nenhum. Fabricado à mão, e só
# num root isolado — num root partilhado seria escrever no estado real.
if [[ "$E2E_ISOLATED" == "1" ]]; then
  mkdir -p "$DELONIX_ROOT/network-gateways"
  printf '{"name":"%s","provider":"native","aliases":[{"name":"dlx_%s","kind":"host","content":["10.99.0.1"]}],"owner":"dlx-0123456789abcdef"}\n' \
    "$GW-legacy" "$PFX" > "$DELONIX_ROOT/network-gateways/$GW-legacy.json"
  check "um registo antigo que nomeia o native apaga-se sem provider" ok \
    "$BIN" delete networkgateways "$GW-legacy"
  check "... e sai do disco" ok test ! -e "$DELONIX_ROOT/network-gateways/$GW-legacy.json"
else
  skip "um registo antigo que nomeia o native apaga-se sem provider" "exige um root isolado (E2E_SHARED_STATE=1)"
fi

cat > "$RWORK/zone.yaml" <<YAML
apiVersion: networking.delonix.io/v1alpha1
kind: NetworkZone
metadata: { name: $ZN }
spec:
  vnets: [{ name: $VN, alias: "e2e $PFX" }]
YAML
if [[ -z "${DELONIX_PROXMOX_URL:-}" ]]; then
  check "NetworkZone sem provider configurado recusa" fail "$BIN" apply -f "$RWORK/zone.yaml"
fi

# ADR-0059 D3: a resolução por nome. Só com NENHUM provider de rede registado,
# para «não registado» ser verdade: o default que nomeia um provider que
# ninguém registou é indisponível (69), nunca um salto para outro; e um
# providers.yaml sem default desliga a regra da contagem (1). A escolha entre
# DOIS providers registados prova-se no teste Rust `resolve::tests` — a
# bateria não a constrói, porque só o Proxmox serve o papel de segmento e o
# providers.yaml aceita uma entrada por tipo (adendo F2c do ADR-0059).
if [[ -z "${DELONIX_PROXMOX_URL:-}" && -z "${DELONIX_OPNSENSE_URL:-}" ]]; then
  printf 'apiVersion: config.delonix.io/v1\nnetworkDefaults:\n  segment: proxmox\n  gateway: opnsense\n' \
    > "$RWORK/providers-def.yaml"
  printf 'apiVersion: config.delonix.io/v1\n' > "$RWORK/providers-nodef.yaml"
  cat > "$RWORK/gw-unnamed.yaml" <<YAML
apiVersion: networking.delonix.io/v1alpha1
kind: NetworkGateway
metadata: { name: $GW-unnamed }
spec:
  aliases: [{ name: dlx_u$PFX, kind: host, content: ["10.99.0.1"] }]
YAML
  check "NetworkZone: um default que nomeia um provider não registado sai com 69" 69 \
    env DELONIX_PROVIDERS_CONFIG="$RWORK/providers-def.yaml" "$BIN" apply -f "$RWORK/zone.yaml"
  check "NetworkGateway sem provider nomeado: o default não registado também sai com 69" 69 \
    env DELONIX_PROVIDERS_CONFIG="$RWORK/providers-def.yaml" "$BIN" apply -f "$RWORK/gw-unnamed.yaml"
  check "um providers.yaml sem networkDefaults.segment desliga a regra da contagem" 1 \
    env DELONIX_PROVIDERS_CONFIG="$RWORK/providers-nodef.yaml" "$BIN" apply -f "$RWORK/zone.yaml"
  check "... e nenhuma destas recusas deixou registo" ok bash -c \
    "! '$BIN' get networkzones 2>/dev/null | grep -q '$ZN' && ! '$BIN' get networkgateways 2>/dev/null | grep -q '$GW-unnamed'"
else
  skip "NetworkZone: um default que nomeia um provider não registado sai com 69" \
    "há um provider de rede configurado: «não registado» não se consegue construir aqui"
fi

# --- OPNsense ---------------------------------------------------------------
if [[ -n "${DELONIX_OPNSENSE_URL:-}" && -n "${DELONIX_OPNSENSE_KEY:-}" && -n "${DELONIX_OPNSENSE_SECRET:-}" ]]; then
  opn() {  # opn <rota> [json] — a mão do operador, SEM a marca do motor
    curl -sk -u "$DELONIX_OPNSENSE_KEY:$DELONIX_OPNSENSE_SECRET" -H 'Content-Type: application/json' \
      -X POST -d "${2:-{\}}" "$DELONIX_OPNSENSE_URL/api/$1"
  }
  opn_rule_uuid() {  # uuid da regra cuja descrição é EXACTAMENTE $1
    opn firewall/filter/search_rule '{"current":1,"rowCount":-1}' | python3 -c \
      'import json,sys; d=sys.argv[1]; print(next((r["uuid"] for r in json.load(sys.stdin)["rows"] if r.get("description")==d), ""))' "$1"
  }
  cat > "$RWORK/gw.yaml" <<YAML
apiVersion: networking.delonix.io/v1alpha1
kind: NetworkGateway
metadata: { name: $GW }
spec:
  provider: opnsense
  aliases: [{ name: dlx_$PFX, kind: host, content: ["10.99.0.1"], description: "e2e $PFX" }]
  rules: [{ description: "e2e $PFX", source: dlx_$PFX, destination: "10.0.0.0/24", protocol: TCP }]
YAML
  check "NetworkGateway aplica alias e regra no OPNsense" ok "$BIN" apply -f "$RWORK/gw.yaml"
  check "... e o segundo apply é idempotente" ok "$BIN" apply -f "$RWORK/gw.yaml"
  check "... a regra no appliance leva a categoria de posse delonix-owner:" ok bash -c \
    "curl -sk -u \"\$DELONIX_OPNSENSE_KEY:\$DELONIX_OPNSENSE_SECRET\" -H 'Content-Type: application/json' -X POST -d '{}' \"\$DELONIX_OPNSENSE_URL/api/firewall/category/search_item\" > '$RWORK/cats.json' && curl -sk -u \"\$DELONIX_OPNSENSE_KEY:\$DELONIX_OPNSENSE_SECRET\" -H 'Content-Type: application/json' -X POST -d '{\"current\":1,\"rowCount\":-1}' \"\$DELONIX_OPNSENSE_URL/api/firewall/filter/search_rule\" | python3 -c 'import json,sys; cats={c[\"uuid\"]:c[\"name\"] for c in json.load(open(\"$RWORK/cats.json\"))[\"rows\"]}; r=[x for x in json.load(sys.stdin)[\"rows\"] if x.get(\"description\")==\"e2e $PFX\"]; sys.exit(0 if r and any(cats.get(u,\"\").startswith(\"delonix-owner:dlx-\") for u in r[0].get(\"categories\",\"\").split(\",\")) else 1)'"
  # Uma regra feita à mão (e APLICADA) com a mesma descrição de outro
  # documento: recusada com 5, nunca adoptada; o teardown desse documento não
  # lhe toca.
  HAND="e2e $PFX hand"
  opn firewall/filter/add_rule "{\"rule\":{\"description\":\"$HAND\",\"source_net\":\"any\",\"destination_net\":\"10.77.0.0/24\"}}" >/dev/null
  opn firewall/filter/apply >/dev/null
  HAND_UUID="$(opn_rule_uuid "$HAND")"
  sed "s/name: $GW }/name: $GW-hand }/; s/description: \"e2e $PFX\", source/description: \"$HAND\", source/; s/name: dlx_$PFX,/name: dlx_${PFX}h,/; s/source: dlx_$PFX,/source: dlx_${PFX}h,/" \
    "$RWORK/gw.yaml" > "$RWORK/gw-hand.yaml"
  check "regra à mão com a mesma descrição: apply recusa com 5 (não adopta)" 5 \
    "$BIN" apply -f "$RWORK/gw-hand.yaml"
  check "delete do documento recusado deixa a regra à mão" ok bash -c \
    "'$BIN' delete networkgateways '$GW-hand' >/dev/null && [ -n '$HAND_UUID' ] && curl -sk -u \"\$DELONIX_OPNSENSE_KEY:\$DELONIX_OPNSENSE_SECRET\" -X POST -H 'Content-Type: application/json' -d '{}' \"\$DELONIX_OPNSENSE_URL/api/firewall/filter/get_rule/$HAND_UUID\" | grep -q '$HAND'"
  # A mesma regra apagada à mão e NÃO aplicada: uma alteração pendente
  # alheia. O apply recusa (5) antes de escrever, e a apagada continua a
  # correr no pf até o operador aplicar.
  opn "firewall/filter/del_rule/$HAND_UUID" >/dev/null
  check "com uma alteração pendente alheia no appliance, o apply recusa com 5" 5 \
    "$BIN" apply -f "$RWORK/gw.yaml"
  opn firewall/filter/apply >/dev/null
  check "delete do NetworkGateway tira o alias e a regra dele" ok \
    "$BIN" delete networkgateways "$GW"
  # The search has to ANSWER (a `rows` list) for "no such rule" to mean
  # anything: an appliance that is down gives an empty body, not a clean one.
  check "... e no appliance não sobra a regra dele (pesquisa respondida, regra ausente)" ok bash -c \
    "curl -skf -m 20 -u \"\$DELONIX_OPNSENSE_KEY:\$DELONIX_OPNSENSE_SECRET\" -H 'Content-Type: application/json' -X POST -d '{\"current\":1,\"rowCount\":-1}' \"\$DELONIX_OPNSENSE_URL/api/firewall/filter/search_rule\" | python3 -c 'import json,sys; rows=json.load(sys.stdin)[\"rows\"]; sys.exit(1 if any(r.get(\"description\")==\"e2e $PFX\" for r in rows) else 0)'"
else
  for n in "NetworkGateway aplica no OPNsense com a marca de posse" \
           "regra à mão com a mesma descrição é recusada, não adoptada" \
           "delete tira só o que tem a marca"; do
    skip "$n" "sem DELONIX_OPNSENSE_URL/_KEY/_SECRET: não há appliance OPNsense, a posse não foi medida aqui"
  done
fi

# --- Proxmox SDN ------------------------------------------------------------
# The token reaches the engine from DELONIX_PROXMOX_TOKEN_FILE or from
# DELONIX_PROXMOX_TOKEN. The file is the form to use for a whole battery: with
# the variable the engine warns on stderr in EVERY command (the secret is
# inherited by every child), and the checks elsewhere that read `2>&1` fail on
# that line. Measured 2026-10-03: 3 FAIL with the variable, none with the file.
# The operator's hand below (curl) reads the same secret from a 0600 header
# file, so it is never on a command line.
PVE_SECRET="${DELONIX_PROXMOX_TOKEN:-}"
if [[ -z "$PVE_SECRET" && -r "${DELONIX_PROXMOX_TOKEN_FILE:-}" ]]; then
  PVE_SECRET="$(tr -d '\r\n' < "$DELONIX_PROXMOX_TOKEN_FILE")"
fi
if [[ -n "${DELONIX_PROXMOX_URL:-}" && -n "${DELONIX_PROXMOX_TOKEN_ID:-}" && -n "$PVE_SECRET" ]]; then
  PVE_HDR="$RWORK/pve-auth.hdr"
  ( umask 077; printf 'Authorization: PVEAPIToken=%s=%s\n' "$DELONIX_PROXMOX_TOKEN_ID" "$PVE_SECRET" > "$PVE_HDR" )
  pve() {  # pve <método> <caminho> [--data ...] — a mão do operador
    local m="$1" p="$2"; shift 2
    curl -sk -X "$m" -H @"$PVE_HDR" \
      "$DELONIX_PROXMOX_URL/api2/json$p" "$@"
  }
  check "NetworkZone aplica zona e vnet no cluster" ok "$BIN" apply -f "$RWORK/zone.yaml"
  check "... e o segundo apply é idempotente" ok "$BIN" apply -f "$RWORK/zone.yaml"
  check "... a vnet leva a marca de posse no alias" ok bash -c \
    "curl -sk -H @'$PVE_HDR' \"\$DELONIX_PROXMOX_URL/api2/json/cluster/sdn/vnets/$VN\" | grep -q 'delonix-owner:dlx-'"
  # An absence check proves nothing unless the same read can see the zone
  # while it exists, and unless the read answered: `pve_zone_state` prints
  # present, absent, or unread, and only the first two are answers.
  pve_zone_state() {  # pve_zone_state <zone>
    curl -skf -m 20 -H @"$PVE_HDR" "$DELONIX_PROXMOX_URL/api2/json/cluster/sdn/zones" | python3 -c \
      'import json,sys
try: d=json.load(sys.stdin)["data"]
except Exception: print("unread"); sys.exit(0)
print("present" if any(z.get("zone")==sys.argv[1] for z in d) else "absent")' "$1"
  }
  export -f pve_zone_state; export PVE_HDR
  check "... e a leitura da lista de zonas vê a zona dele no cluster" ok bash -c \
    "[ \"\$(pve_zone_state '$ZN')\" = present ]"
  # Uma zona que já existe e que o motor não criou: recusada, nunca adoptada.
  HZ="h$(( $$ % 100000 ))"
  pve POST /cluster/sdn/zones --data-urlencode "zone=$HZ" --data-urlencode type=simple >/dev/null
  pve PUT /cluster/sdn >/dev/null; sleep 3
  sed "s/name: $ZN }/name: $HZ }/; s/name: $VN,/name: ${VN}h,/" "$RWORK/zone.yaml" > "$RWORK/zone-hand.yaml"
  check "zona feita à mão com o mesmo nome: apply recusa com 5 (não adopta)" 5 \
    "$BIN" apply -f "$RWORK/zone-hand.yaml"
  check "delete desse documento deixa a zona à mão de pé" ok bash -c \
    "'$BIN' delete networkzones '$HZ' >/dev/null; curl -sk -H @'$PVE_HDR' \"\$DELONIX_PROXMOX_URL/api2/json/cluster/sdn/zones/$HZ\" | grep -q '\"zone\":\"$HZ\"'"
  pve DELETE "/cluster/sdn/zones/$HZ" >/dev/null; pve PUT /cluster/sdn >/dev/null; sleep 3
  # Uma alteração pendente de outro (staged, não aplicada): o apply recusa
  # ANTES de escrever, e a alteração do outro continua pendente, não aplicada.
  pve POST /cluster/sdn/zones --data-urlencode "zone=$HZ" --data-urlencode type=simple >/dev/null
  check "com uma alteração SDN pendente alheia, o apply recusa com 5" 5 \
    "$BIN" apply -f "$RWORK/zone.yaml"
  check "... e a alteração alheia continua pendente (não foi empurrada)" ok bash -c \
    "curl -sk -H @'$PVE_HDR' \"\$DELONIX_PROXMOX_URL/api2/json/cluster/sdn/zones?pending=1\" | python3 -c 'import json,sys; z=[x for x in json.load(sys.stdin)[\"data\"] if x.get(\"zone\")==\"$HZ\"]; sys.exit(0 if z and z[0].get(\"state\")==\"new\" else 1)'"
  pve POST /cluster/sdn/rollback >/dev/null
  check "delete do NetworkZone tira a vnet e a zona dele" ok "$BIN" delete networkzones "$ZN"
  check "... e a zona já não existe no cluster (lista lida, zona ausente)" ok bash -c \
    "[ \"\$(pve_zone_state '$ZN')\" = absent ]"
else
  for n in "NetworkZone aplica zona e vnet com a marca de posse" \
           "zona à mão com o mesmo nome é recusada, não adoptada" \
           "alteração SDN pendente alheia recusa o apply" \
           "delete tira só a zona e as vnets do motor"; do
    skip "$n" "sem DELONIX_PROXMOX_URL/_TOKEN_ID e _TOKEN_FILE (ou _TOKEN): não há cluster Proxmox, a posse na SDN não foi medida aqui"
  done
fi

section "kind: SystemContainer — as recusas que não precisam de nó (ADR-0058, plano 63)"
########################################
# O que se prova sem um nó Proxmox. O ciclo com o nó (criar, deriva à mão,
# convergir a quente, recriar, apagar) é o portão ao vivo da Fatia 4 e o caso
# `a_system_container_runs_its_lifecycle_through_the_node` do live.rs. A recusa
# de uma layer zstd (DX-1409) é o teste `a_zstd_or_foreign_layer_is_refused_by_name`
# do delonix-oci: pelo Kind só se chega lá com um nó configurado.
SCWORK="$OUT/sc-$PFX"; mkdir -p "$SCWORK"
cat > "$SCWORK/priv.yaml" <<YAML
apiVersion: compute.delonix.io/v1alpha1
kind: SystemContainer
metadata: { name: sc-$PFX-priv }
spec: { image: alpine:3.20, unprivileged: false }
YAML
cat > "$SCWORK/plain.yaml" <<YAML
apiVersion: compute.delonix.io/v1alpha1
kind: SystemContainer
metadata: { name: sc-$PFX }
spec: { image: alpine:3.20 }
YAML
# Antes desta fatia, `unprivileged: false` era «campo desconhecido — ignorado»,
# e com um nó configurado o apply criava um container SEM privilégio com
# código 0: o contrário do que o manifesto pedia. A recusa corre antes de
# resolver o provider, por isso sem nó ela ganha ao DX-6000.
check "stack apply com unprivileged: false recusa com DX-1540, antes de procurar o nó" ok bash -c \
  "$adr54_fns; adr54_bare stack apply -f '$SCWORK/priv.yaml' 2>&1 | grep -q 'DX-1540'"
check "... com a classe inválido (1)" 1 adr54_bare stack apply -f "$SCWORK/priv.yaml"
check "stack plan também recusa o campo de privilégio (DX-1540)" ok bash -c \
  "$adr54_fns; adr54_bare stack plan -f '$SCWORK/priv.yaml' 2>&1 | grep -q 'DX-1540'"
check "sem provider configurado, o apply recusa com 69" 69 adr54_bare stack apply -f "$SCWORK/plain.yaml"
check "... e a mensagem diz o que configurar" ok bash -c \
  "$adr54_fns; adr54_bare stack apply -f '$SCWORK/plain.yaml' 2>&1 | grep -q 'DELONIX_PROXMOX_URL'"
check "... e não fica registo para trás" ok bash -c \
  "$adr54_fns; ! adr54_bare get systemcontainers 2>/dev/null | grep -q 'sc-$PFX'"
check "o exec de um system container é declarado unsupported-by-provider" ok bash -c \
  "'$BIN' provider describe proxmox | grep 'system-container.exec' | grep -q 'unsupported-by-provider'"
# Fatia 5 (snapshots): o grupo `systemcontainer` lê o registo antes de ir ao nó,
# por isso um nome não registado responde 4 sem provider nenhum configurado.
#
# These four go through `adr54_env … "$BIN"` and not `adr54_bare`: the CLI
# coverage ratchet (`scripts/cli_exec_ratchet.py`) reads the recorded command
# and does not look inside a shell function, so through `adr54_bare` these
# judged invocations counted as never executed (measured 2026-10-11, S15/N11).
# Same environment, same assertion — only the engine is now named in the argv.
check "systemcontainer snapshot ls de um nome não registado diz 4" 4 adr54_env DELONIX_PROVIDERS_CONFIG="$ADR54_EMPTY" "$BIN" systemcontainer snapshot ls "sc-$PFX-nada"
check "systemcontainer snapshot create de um nome não registado diz 4" 4 adr54_env DELONIX_PROVIDERS_CONFIG="$ADR54_EMPTY" "$BIN" systemcontainer snapshot create "sc-$PFX-nada" s1
check "o snapshot de um system container é declarado supported no proxmox" ok bash -c \
  "'$BIN' provider describe proxmox | grep 'system-container.snapshot' | grep -q 'supported'"
check "o resize de um system container é declarado supported no proxmox" ok bash -c \
  "'$BIN' provider describe proxmox | grep 'system-container.resize' | grep -q 'supported'"
# Fatia 5 (backup): um arquivo do provider é reconhecido pelo nome e o dono é
# procurado no registo antes de ir ao nó, por isso sem registo diz 4.
check "backup restore de um arquivo de container de sistema sem dono registado diz 4" 4 adr54_bare backup restore "local:backup/vzdump-lxc-999-2026_01_01-00_00_00.tar.zst" --force
check "backup create systemcontainer de um nome não registado diz 4" 4 adr54_bare backup create systemcontainer "sc-$PFX-nada"
check "backup create systemcontainer --quiesce é recusado (é só para VMs)" fail adr54_bare backup create systemcontainer "sc-$PFX-nada" --quiesce
check "systemcontainer clone de um nome não registado diz 4" 4 adr54_env DELONIX_PROVIDERS_CONFIG="$ADR54_EMPTY" "$BIN" systemcontainer clone "sc-$PFX-nada" "sc-$PFX-copia"
check "o clone de um system container é declarado supported no proxmox" ok bash -c \
  "'$BIN' provider describe proxmox | grep 'system-container.clone' | grep -q 'supported'"
# Fatia 5 (firewall): `kind: NetworkPolicy` com `scope: systemcontainer` põe a
# política na firewall do container no nó. O alvo é verificado contra os
# SystemContainers declarados e registados antes de qualquer pedido, e um scope
# mal escrito é recusado em vez de cair no `container` por omissão.
cat > "$SCWORK/fw-orfao.yaml" <<YAML
apiVersion: networking.delonix.io/v1alpha1
kind: NetworkPolicy
metadata: { name: sc-$PFX-fw }
spec: { target: sc-$PFX-nada, direction: ingress, scope: systemcontainer, defaultPolicy: deny }
YAML
sed 's/scope: systemcontainer/scope: systemcontainr/' "$SCWORK/fw-orfao.yaml" > "$SCWORK/fw-typo.yaml"
check "stack validate recusa scope: systemcontainer sobre um alvo que não é SystemContainer" 1 adr54_bare stack validate -f "$SCWORK/fw-orfao.yaml"
check "... e a mensagem nomeia o alvo" ok bash -c \
  "$adr54_fns; adr54_bare stack validate -f '$SCWORK/fw-orfao.yaml' 2>&1 | grep -q \"target 'sc-$PFX-nada' is not a declared or registered SystemContainer\""
check "um scope mal escrito é recusado e a mensagem lista os válidos" ok bash -c \
  "$adr54_fns; adr54_bare stack validate -f '$SCWORK/fw-typo.yaml' 2>&1 | grep -q 'container|network|vm|systemcontainer'"
check "a firewall de um system container é declarada partial no proxmox" ok bash -c \
  "'$BIN' provider describe proxmox | grep 'system-container.firewall' | grep -q 'partial'"
check "systemcontainer move de um nome não registado diz 4" 4 adr54_env DELONIX_PROVIDERS_CONFIG="$ADR54_EMPTY" "$BIN" systemcontainer move "sc-$PFX-nada" --node pve2
check "systemcontainer move sem --node é recusado pelo parser" 2 adr54_bare systemcontainer move "sc-$PFX-nada"
check "a migração de um system container é declarada supported no proxmox" ok bash -c \
  "'$BIN' provider describe proxmox | grep 'system-container.move' | grep -q 'supported'"

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
  e2e_rm "$TTYC"
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
section "delete networkpolicies — um DELETE que reporta sucesso tem de apagar"
########################################
# Medido 2026-10-06 contra o motor publicado: com um `deny` em vigor,
# `delete networkpolicies <c>/ingress` respondia rc 0 e «removed 0 inbound
# rule(s)», o `get`/`describe networkpolicies` continuavam a ler `deny`, e um
# ping de outro container CONTINUAVA bloqueado. O operador apaga a política que
# fecha o workload e ele fica fechado, com todas as leituras a concordar com a
# política em vez de com ele.
#
# A causa: o verbo genérico chamava a limpeza do `net ingress clear`, que remove
# as REGRAS de uma direcção e deixa a omissão ao verbo `policy` — coerente para
# esse verbo, e é o que a mensagem dele promete. Aqui o objecto apagado É a
# política, por isso a omissão tem de ir com ela.
#
# O veredicto é o TRÁFEGO, contado como echo requests RECEBIDOS pelo alvo
# (`Icmp InEchos`) e não pelo rc do ping: um ping cuja resposta é dropada também
# falha, e um check pelo rc passaria sobre um defeito que entrega o pacote.
DP="dp-$PFX"; DPS="dps-$PFX"; DPC="dpc-$PFX"
if check_then "delete networkpolicies: a rede cria-se" ok "$BIN" network create "$DP" \
   && check_then "delete networkpolicies: o servidor arranca" ok \
        "$BIN" container run -d --name "$DPS" --net "$DP" "$IMG" sleep 900 \
   && check_then "delete networkpolicies: o cliente arranca" ok \
        "$BIN" container run -d --name "$DPC" --net "$DP" "$IMG" sleep 900; then
  DPIP=$(ip_of "$DPS")
  e0=$(echos_of "$DPS"); "$BIN" container exec "$DPC" ping -c3 -W1 "$DPIP" >/dev/null 2>&1
  check "delete policy: a linha de base alcança" ok test "$(( $(echos_of "$DPS") - e0 ))" -eq 3
  check "net ingress policy deny" ok "$BIN" net ingress policy "$DPS" deny
  e0=$(echos_of "$DPS"); "$BIN" container exec "$DPC" ping -c3 -W1 "$DPIP" >/dev/null 2>&1
  check "delete policy: com deny não alcança" ok test "$(( $(echos_of "$DPS") - e0 ))" -eq 0
  check "delete networkpolicies <c>/ingress" ok "$BIN" delete networkpolicies "$DPS/ingress"
  check "... e o get passa a ler allow-all" ok bash -c \
    "'$BIN' get networkpolicies 2>&1 | grep -qE '^$DPS +ingress +allow'"
  check "... e o describe também" ok bash -c \
    "'$BIN' describe networkpolicies '$DPS/ingress' 2>&1 | grep -q 'default policy: allow'"
  e0=$(echos_of "$DPS"); "$BIN" container exec "$DPC" ping -c3 -W1 "$DPIP" >/dev/null 2>&1
  check "... e o deny DEIXOU de ser imposto no fio" ok test "$(( $(echos_of "$DPS") - e0 ))" -eq 3
  # `net ingress clear` continua a ser o que a sua mensagem diz: leva as REGRAS
  # e deixa a omissão de pé. Sem este check, alinhar os dois verbos passaria
  # por correcção.
  "$BIN" net ingress policy "$DPS" deny >/dev/null 2>&1
  check "net ingress clear" ok "$BIN" net ingress clear "$DPS"
  check "net ingress clear: a omissão da direcção FICA (deny)" ok bash -c \
    "'$BIN' get networkpolicies 2>&1 | grep -qE '^$DPS +ingress +deny'"
  "$BIN" delete networkpolicies "$DPS/ingress" >/dev/null 2>&1
fi
for c in "$DPS" "$DPC"; do e2e_rm "$c"; done
e2e_net_rm "$DP"

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
  "grep -q 'memory: 512M' '$SCAFDIR/$SCAFN/delonix-manifest.yaml'"
# Os templates de edge (nginx/httpd/haproxy) nascem com HTTPS: o `init` gera o
# certificado em ./tls (mkcert se houver, senão auto-assinado pelo próprio
# binário), a chave fica 0600 e nunca chega à imagem nem ao git.
check "o init gera o certificado e a chave privada fica 0600" ok bash -c \
  "[ -s '$SCAFDIR/$SCAFN/tls/tls.crt' ] && [ \"\$(stat -c %a '$SCAFDIR/$SCAFN/tls/tls.key')\" = 600 ]"
check "a chave não entra na imagem nem no git (.dockerignore e .gitignore)" ok bash -c \
  "grep -qx 'tls/' '$SCAFDIR/$SCAFN/.dockerignore' && grep -qx 'tls/' '$SCAFDIR/$SCAFN/.gitignore'"
check "nenhum token por substituir no projecto gerado" ok bash -c \
  "! grep -rqE '__(PORT|TLS_PORT|NAME|TEMPLATE_VERSION)__' '$SCAFDIR/$SCAFN'"
# As portas e os nomes do certificado respondem-se por flag, sem terminal.
check "init --port/--tls-port/--hostname entram no projecto e no certificado" ok bash -c \
  "'$BIN' stack init -t nginx --port 19080 --tls-port 19443 --hostname shop.test '$SCAFDIR/flags-$PFX' >/dev/null 2>&1 \
   && grep -q '\"19080:19080\"' '$SCAFDIR/flags-$PFX/delonix-manifest.yaml' && grep -q '\"19443:19443\"' '$SCAFDIR/flags-$PFX/delonix-manifest.yaml' \
   && grep -q 'listen      19443 ssl' '$SCAFDIR/flags-$PFX/nginx.conf' \
   && { ! command -v openssl >/dev/null || openssl x509 -in '$SCAFDIR/flags-$PFX/tls/tls.crt' -noout -ext subjectAltName | grep -q 'shop.test'; }"
check "--tls-port num template sem TLS é recusado e não escreve nada" ok bash -c \
  "! '$BIN' stack init -t go --tls-port 19443 '$SCAFDIR/noflags-$PFX' >/dev/null 2>&1 && [ ! -e '$SCAFDIR/noflags-$PFX' ]"

# ---- uma porta PRIVILEGIADA (bug de produção, 2026-10-15) ----------------
# Reportado: `delonix init -t nginx --port 9000 --tls-port 943` gerava o
# projecto e o build morria sempre em `nginx: [emerg] bind() to 0.0.0.0:943
# failed (13: Permission denied)`. Medido: o container de trabalho do build
# partilha a netns do HOST (mesmo inode), onde a nossa userns não tem
# CAP_NET_BIND_SERVICE, e `nginx -t` BINDA cada `listen` que lê. Uma porta
# apenas OCUPADA passa (o `nginx -t` tolera EADDRINUSE) — é o privilégio que
# morde, por isso a porta do teste sai do limiar REAL deste host.
LOWTHR=$(cat /proc/sys/net/ipv4/ip_unprivileged_port_start 2>/dev/null || echo 1024)
if [ "${LOWTHR:-0}" -gt 1 ] 2>/dev/null; then
  LOWP=$((LOWTHR - 1))
  # 1) O aviso sai no momento em que a flag é dada — a maioria dos projectos é
  #    gerada agora e aplicada depois, onde a recusa chegaria tarde demais.
  # A saída é CAPTURADA antes de se procurar nela: `| grep -q` sai no primeiro
  # casamento, fecha o pipe e mata o gerador com SIGPIPE — o aviso sai cedo, por
  # isso o projecto ficava por escrever e o build seguinte não tinha o que construir.
  check "init: uma porta que este host não publica é dita ao gerar" ok bash -c \
    "out=\$('$BIN' stack init -t nginx --port 19082 --tls-port $LOWP --hostname lp '$SCAFDIR/low-$PFX' 2>&1); \
     printf '%s' \"\$out\" | grep -q -- '--tls-port $LOWP needs privilege to bind'"
  # 2) E o build do projecto gerado PASSA: o check de config deixou de bindar
  #    as portas reais. É este o bug reportado, medido pelo efeito.
  #    Só o 124 do `timeout` é ambiente; QUALQUER outro rc é a falha que este
  #    check existe para apanhar e tem de aparecer como FAIL, não como SKIP —
  #    o binário publicado morre aqui com `bind() ... Permission denied`.
  LOWLOG="${TMPDIR:-/tmp}/e2e-lowport-build.log"
  timeout 400 "$BIN" build -t "low-$PFX:dev" "$SCAFDIR/low-$PFX" >"$LOWLOG" 2>&1
  LOWRC=$?
  if [ "$LOWRC" = 124 ]; then
    skip "build com porta privilegiada" \
      "o build não completou em 400s neste ambiente — ver $LOWLOG"
  else
    check "build: o check de config do nginx não binda a porta privilegiada" ok bash -c \
      "[ '$LOWRC' = 0 ] && grep -q 'test is successful' '$LOWLOG' && ! grep -q 'Permission denied' '$LOWLOG'"
    check "build: e o check ainda corre mesmo (não foi removido)" ok bash -c \
      "grep -q 'syntax is ok' '$LOWLOG'"
    check "teardown: the privileged-port build leaves the store" ok e2e_image_rm "low-$PFX:dev"
  fi
  # 3) `--up` recusa ANTES do build: achada só no `stack apply`, custa o build
  #    inteiro primeiro — a mesma razão que já justifica a recusa de porta ocupada.
  #    O rc sozinho não distingue nada (o binário publicado também sai 1, só que
  #    depois de o build inteiro ter falhado), por isso o que se julga é a RAZÃO
  #    e a ausência de qualquer build.
  LOWUPLOG="${TMPDIR:-/tmp}/e2e-lowport-up.log"
  timeout 300 "$BIN" stack init -t nginx --port 19083 --tls-port "$LOWP" --hostname lp --up \
    "$SCAFDIR/lowup-$PFX" >"$LOWUPLOG" 2>&1
  check "--up: a recusa é a porta, não uma falha do build" ok bash -c \
    "grep -q -- '--tls-port $LOWP needs privilege to bind' '$LOWUPLOG' && ! grep -q 'RUN failed' '$LOWUPLOG'"
  check "--up: o build nem chegou a começar" ok bash -c \
    "! grep -q 'Building image' '$LOWUPLOG' && ! '$BIN' image ls | grep -q 'lowup-$PFX'"
else
  skip "porta privilegiada no init" \
    "este host não tem portas privilegiadas (ip_unprivileged_port_start=$LOWTHR) — não há o que exercitar"
fi

# ---- o template que `--up` constrói é o que foi PEDIDO -------------------
# Reportado no mesmo dia: `delonix init -t odoo <dir>` sobre um directório que
# já tinha um projecto nginx escreveu nada, disse «nothing to do (everything
# already existed)» — e `--up` foi construir e arrancar o projecto NGINX,
# à espera do health path do odoo, na porta do odoo.
FGN="$SCAFDIR/foreign-$PFX"
check "um projecto nginx para a pergunta seguinte" ok \
  "$BIN" stack init -t nginx --port 19085 --tls-port 19446 --hostname fg "$FGN"
check "init -t <outro> sobre um projecto existente é recusado (classe 5)" 5 \
  "$BIN" stack init -t odoo --port 19086 "$FGN"
check "a recusa nomeia o que está lá E o que foi pedido" ok bash -c \
  "out=\$('$BIN' stack init -t odoo --port 19086 '$FGN' 2>&1); \
   printf '%s' \"\$out\" | grep -q 'FROM nginx' && printf '%s' \"\$out\" | grep -q 'FROM odoo'"
check "o mesmo template com as mesmas flags continua a passar (2.º --up idempotente)" ok \
  "$BIN" stack init -t nginx --port 19085 --tls-port 19446 --hostname fg "$FGN"
check "uma config editada à mão NÃO é glue e não trava nada" ok bash -c \
  "printf 'worker_processes 1;\\n' >> '$FGN/nginx.conf' && \
   '$BIN' stack init -t nginx --port 19085 --tls-port 19446 --hostname fg '$FGN'"
# The guard is the HOST's ports, read with `ss` before the engine is asked. It
# used to be the `--up` itself: any failure SKIPped as «rede lenta, ou porta
# 8080/8443 já ocupada», so a broken build or apply read as a busy host. Measured
# 2026-10-21 on the dev host, where it was the port (8443 held by the host's
# nginx) — the guard now says so by name, and a failure on free ports is a FAIL.
_scaf_busy=$(ss -tlnpH 2>/dev/null | awk '$4 ~ /:(8080|8443)$/ {print $4, $6}' | tr '\n' ' ')
if [[ -n "$_scaf_busy" ]]; then
  skip "stack init --template httpd --up" "as portas do template já estão ocupadas neste host: $_scaf_busy"
elif check_then "--up: stack init --template httpd constrói e aplica (180 s)" ok bash -c \
    "timeout 180 '$BIN' stack init --template httpd '$SCAFDIR/$SCAFN' --up --force >'${TMPDIR:-/tmp}/e2e-scaffold-up.log' 2>&1 \
     || { rc=\$?; tail -15 '${TMPDIR:-/tmp}/e2e-scaffold-up.log'; exit \$rc; }"; then
  check "--up: o container tem memory_max do manifesto (não só o run cru)" ok bash -c \
    "$BIN container inspect '$SCAFN' | grep -q '\"memory_max\": \"512M\"'"
  check "--up: o HTTPS responde, com HTTP/2, e o HTTP redirecciona para lá" ok bash -c \
    "[ \"\$(curl -ks -o /dev/null -w '%{http_code} %{http_version}' https://127.0.0.1:8443/)\" = '200 2' ] && curl -s -o /dev/null -D - http://127.0.0.1:8080/x | grep -qi '^location: https://127.0.0.1:8443/x'"
  check "--up: o resumo final dá o endereço HTTPS a abrir" ok \
    grep -q 'open:    https://localhost:8443/' "${TMPDIR:-/tmp}/e2e-scaffold-up.log"
  check "--up: o smoke do próprio template passa" ok bash -c \
    "cd '$SCAFDIR/$SCAFN' && PATH=\"$(dirname "$BIN"):\$PATH\" sh scripts/smoke.sh"
  check "--up: o container está mesmo a correr" ok bash -c \
    "$BIN container ls | grep -q '$SCAFN'"
  # Um segundo `--up` sobre o projecto que já está de pé. Medido 2026-10-02: a
  # verificação de portas recusava-o a nomear o slirp do PRÓPRIO projecto como
  # «outro processo»; e, passada essa, um projecto alterado e reconstruído
  # ficava «is UP» com o container antigo a servir a página anterior.
  _cid1=$("$BIN" container inspect "$SCAFN" 2>/dev/null | grep -m1 '"id"')
  check "--up outra vez, sem alterações: passa e o container é o mesmo" ok bash -c \
    "timeout 180 '$BIN' stack init --template httpd '$SCAFDIR/$SCAFN' --up --force >/dev/null 2>&1 \
     && [ \"\$('$BIN' container inspect '$SCAFN' | grep -m1 '\"id\"')\" = '$_cid1' ]"
  echo '<h1>e2e second build</h1>' > "$SCAFDIR/$SCAFN/public/index.html"
  check "--up depois de alterar o projecto: serve a versão nova" ok bash -c \
    "timeout 420 '$BIN' stack init --template httpd '$SCAFDIR/$SCAFN' --up --force >/dev/null 2>&1 \
     && curl -ks https://127.0.0.1:8443/ | grep -q 'e2e second build'"
  "$BIN" stack destroy -f "$SCAFDIR/$SCAFN/delonix-manifest.yaml" >/dev/null 2>&1
else
  e2e_rm "$SCAFN"
fi
rm -rf "$SCAFDIR"

section "leituras e ciclos locais que nunca eram executados (F0.2: a metade da cobertura)"
########################################
# As 272 folhas têm o `--help` verificado por construção; quantas a bateria
# EXECUTA é o outro número, e desde 2026-10-07 é um portão
# (`scripts/cli_exec_ratchet.py`). Esta secção fecha a parte BARATA do que
# faltava: leituras puras e ciclos locais que não precisam de hipervisor, de
# rede, nem de privilégio.
#
# Cada check assere o INVARIANTE que o comando promete, nunca só que devolveu
# 0. Um check que invoca e não julga nada sobe o numerador do portão e não
# testa nada — é a desonestidade que o portão existe para impedir, e seria
# pior que a folha continuar a zero.

# --- `version` imprime o texto da flag VERBATIM ------------------------------
# A razão de o comando existir a par da flag está no AGENTS.md: `<ferramenta>
# version` é o que se escreve primeiro. E a promessa escrita é que imprime o
# texto da flag verbatim, «para os dois não poderem divergir» — nada verificava
# isso, e uma divergência não daria erro nenhum: daria duas respostas à mesma
# pergunta.
check "version: byte a byte igual ao --version" ok bash -c "
  diff <('$BIN' version 2>&1) <('$BIN' --version 2>&1) >/dev/null"

# --- `compatibility docker`: os três estados, como o `compose` já exige ------
# O `compatibility compose` já tem secção própria (M02). O `docker` nunca era
# executado, e é o mesmo contrato: servido, recusado COM razão, em falta.
check "compatibility docker: conta o servido e o recusado, e nomeia o mapeamento" ok bash -c "
  out=\$('$BIN' compatibility docker 2>&1) || exit 1
  grep -qE '[0-9]+ served, [0-9]+ refused with a reason' <<<\"\$out\" || { echo \"sem a linha de contagem\"; exit 1; }
  grep -q 'MAPS TO' <<<\"\$out\" || { echo 'sem a coluna do mapeamento'; exit 1; }
  grep -q '/_ping' <<<\"\$out\" || { echo 'sem a rota de liveness'; exit 1; }"

# --- `config`: o ciclo, e as DUAS recusas pelo nome --------------------------
# O `--help` do `config set` promete «refuses an unknown key or an invalid
# value BY NAME — never accepted-and-ignored». As duas recusas são a parte que
# um aceite-e-ignorado silencioso trairia.
check "config: set/get/unset fecham o ciclo, e o get volta ao default" ok bash -c "
  '$BIN' config set output json >/dev/null || exit 1
  [ \"\$('$BIN' config get output)\" = json ] || { echo 'o get não devolveu o que o set escreveu'; exit 1; }
  '$BIN' config unset output >/dev/null || exit 1
  [ \"\$('$BIN' config get output)\" = '(unset)' ] || { echo 'o unset não repôs o default'; exit 1; }"
# As duas recusas são asseridas DENTRO do corpo, com a classe: um `fail` só
# prova que recusou, e um corpo que acaba num `grep` bem-sucedido devolve 0 —
# foi exactamente assim que a primeira versão destes dois checks se enganou a
# si própria (esperava `fail` num corpo que passa a 0 quando está tudo certo).
# Por fora do check, de propósito: se o ciclo acima chumbar a meio deixa
# `output = json` escrito, e esse valor é GLOBAL — todos os checks seguintes
# passariam a receber JSON onde esperam tabela, e a bateria daria uma cascata de
# FAIL cuja causa está centenas de linhas atrás.
"$BIN" config unset output >/dev/null 2>&1

check "config set: uma chave desconhecida é recusada PELO NOME, classe 1" ok bash -c "
  out=\$('$BIN' config set nao.existe 1 2>&1); rc=\$?
  [ \$rc -eq 1 ] || { echo \"devia recusar com 1, deu \$rc\"; exit 1; }
  grep -q 'is not a config key' <<<\"\$out\" || { echo 'recusou sem nomear a chave'; exit 1; }"
check "config set: um valor inválido é recusado, e diz os aceites" ok bash -c "
  out=\$('$BIN' config set output xpto 2>&1); rc=\$?
  [ \$rc -eq 1 ] || { echo \"devia recusar com 1, deu \$rc\"; exit 1; }
  grep -q 'table|json' <<<\"\$out\" || { echo 'recusou sem dizer os valores aceites'; exit 1; }"

# --- `provider config show`: de onde vem cada valor, e nunca o segredo -------
# O que este comando promete é a PRECEDÊNCIA: o ficheiro em vigor, e o que a
# precedência ignorou. A bateria aponta o `DELONIX_PROVIDERS_CONFIG` para um
# ficheiro próprio, logo o `/etc/delonix/providers.yaml` do host tem de
# aparecer como IGNORADO — se não aparecer, a corrida estava a ler a máquina.
check "provider config show: nomeia o ficheiro em vigor E o que a precedência ignora" ok bash -c "
  out=\$('$BIN' provider config show 2>&1) || exit 1
  grep -qF \"File:\" <<<\"\$out\" || { echo 'não diz qual o ficheiro'; exit 1; }
  grep -qF \"\$DELONIX_PROVIDERS_CONFIG\" <<<\"\$out\" || { echo 'não nomeia o ficheiro desta corrida'; exit 1; }
  grep -qE 'Default provider: .+ \((file|built-in|env|auto-detection)\)' <<<\"\$out\" || { echo 'não diz a ORIGEM do default'; exit 1; }"

# --- `system`: o que o host tem, e o que o motor consegue impor --------------
check "system resources: nomeia os controladores e o que seria IGNORADO" ok bash -c "
  out=\$('$BIN' system resources 2>&1) || exit 1
  grep -q 'cpus:' <<<\"\$out\""
check "system virt: responde sobre a virtualização do host" ok "$BIN" system virt
check "system setup: diz o MODO em que o motor corre" ok bash -c "
  '$BIN' system setup 2>&1 | grep -qE 'mode: +(rootless|root)'"
check "system metrics: as contagens do nó" ok bash -c "
  '$BIN' system metrics 2>&1 | grep -q 'Containers:'"

# --- namespaces: a listagem deriva do que está EM USO ------------------------
check "system namespace ls: lista a namespace default" ok bash -c "
  '$BIN' system namespace ls 2>&1 | grep -q '^default'"
check "system namespace describe: nomeia o SET de isolamento, não só o nome" ok bash -c "
  '$BIN' system namespace describe default 2>&1 | grep -q 'Isolation set: *dlxns'"

# --- `policy unset` sem política: diz que não há, e devolve 0 ----------------
# Um `unset` do que não existe não é um erro — é um no-op, e tem de o dizer.
check "policy unset: sem política declarada diz que não há nada a remover" ok bash -c "
  '$BIN' policy unset -f 2>&1 | grep -q 'no runtime policy is set'"

# --- `manifest`: validar e renderizar -----------------------------------------
check "manifest validate: um exemplo publicado resolve todas as referências" ok bash -c "
  '$BIN' manifest validate -f '$REPO/examples/netroute.yaml' 2>&1 | grep -q 'all references resolved'"
check "manifest validate: um apiVersion desconhecido é recusado pelo nome" ok bash -c "
  printf 'apiVersion: x\nkind: Network\nmetadata:\n  name: a\n' >'$OUT/bad-manifest.yaml'
  out=\$('$BIN' manifest validate -f '$OUT/bad-manifest.yaml' 2>&1); rc=\$?
  [ \$rc -ne 0 ] || { echo 'aceitou um apiVersion que o motor não conhece'; exit 1; }
  grep -q \"unknown apiVersion 'x'\" <<<\"\$out\" || { echo 'recusou sem nomear o apiVersion'; exit 1; }"
check "manifest render: materializa os defaults que o documento não escreve" ok bash -c "
  out=\$('$BIN' manifest render -f '$REPO/examples/netroute.yaml' 2>&1) || exit 1
  grep -q '^kind: Network' <<<\"\$out\" || { echo 'não renderizou o Kind'; exit 1; }
  grep -q 'driver:' <<<\"\$out\" || { echo 'não materializou o default do driver'; exit 1; }"

# --- `completion editor`: escreve os ficheiros que nomeia ---------------------
check "completion editor vim: os ficheiros que a saída nomeia existem mesmo" ok bash -c "
  d='$OUT/ed-$PFX'; rm -rf \"\$d\"
  out=\$('$BIN' completion editor vim --dir \"\$d\" 2>&1) || exit 1
  while read -r f; do [ -s \"\$f\" ] || { echo \"nomeado e não escrito: \$f\"; exit 1; }; done <<<\"\$out\""

# --- segredos: o ciclo, e o invariante do rotate-key -------------------------
# `rotate-key` recifra TUDO com uma chave nova. O invariante que importa é o
# que não se vê: os valores em claro têm de ler-se IGUAIS depois. Uma rotação
# que perde o plaintext é a pior falha possível deste subsistema, e seria
# invisível a um check que só olhasse para o rc.
check "secret apply: cria da versão 1, e AVISA do cleartext no manifesto" ok bash -c "
  printf 'apiVersion: core.delonix.io/v1alpha1\nkind: Secret\nmetadata:\n  name: sec-$PFX\nspec:\n  stringData:\n    k: valor-fixo\n' >'$OUT/sec-$PFX.yaml'
  out=\$('$BIN' secret apply -f '$OUT/sec-$PFX.yaml' 2>&1) || exit 1
  grep -q 'CLEARTEXT' <<<\"\$out\" || { echo 'não avisou do cleartext'; exit 1; }
  '$BIN' secret inspect sec-$PFX --reveal 2>&1 | grep -q 'k=valor-fixo'"
check "secret rotate: troca o VALOR da chave, e sobe a versão" ok bash -c "
  '$BIN' secret rotate sec-$PFX k >/dev/null || exit 1
  '$BIN' secret inspect sec-$PFX --reveal 2>&1 | grep -q 'k=valor-fixo' && { echo 'o valor não mudou'; exit 1; }
  '$BIN' secret inspect sec-$PFX --reveal 2>&1 | grep -q 'k='"
check "secret rotate-key: recifra tudo e os valores em claro continuam os MESMOS" ok bash -c "
  '$BIN' secret set sec-$PFX k=valor-fixo >/dev/null || exit 1
  antes=\$('$BIN' secret inspect sec-$PFX --reveal 2>&1 | grep 'k=')
  '$BIN' secret rotate-key >/dev/null || exit 1
  depois=\$('$BIN' secret inspect sec-$PFX --reveal 2>&1 | grep 'k=')
  [ \"\$antes\" = \"\$depois\" ] || { echo \"a rotação da chave-mestra perdeu o valor: '\$antes' -> '\$depois'\"; exit 1; }"
# --- ACH-034: a rotação da chave-mestra punha a VERSÃO a 1 -------------------
# Medido 2026-10-08, com o check acima a passar: os valores sobreviviam e a
# VERSÃO recuava (4 -> 1). A regra da versão tem um dono só e está escrita no
# `SecretStore::save` (ADR-0069 item 6): valores iguais -> mesma versão;
# valores diferentes -> a anterior + 1; primeira gravação -> 1. Uma rotação de
# chave não muda valor nenhum, logo pela regra do próprio store a versão tem
# de ficar igual.
#
# A causa era a ORDEM dentro do `SecretStore::rotate_key`: rodava a
# chave-mestra ANTES de regravar, e o `save` de cada segredo comparava contra
# um ficheiro ainda selado com a chave ANTIGA, caía no ramo «primeira
# gravação» e escrevia 1.
#
# Corrigido em bbf2a972 (o `rotate_key` regrava com a versão que já leu). Foi
# `xfail` até lá e passou a check normal quando deu XPASS; a asserção pede
# IGUAL, que é o que a regra diz, e não apenas «não recua».
check "secret rotate-key: a versão fica igual (os valores não mudaram)" ok bash -c "
  '$BIN' secret set sec-$PFX k=para-a-versao >/dev/null || exit 1
  '$BIN' secret set sec-$PFX k=outro-valor >/dev/null || exit 1
  antes=\$('$BIN' secret inspect sec-$PFX 2>&1 | sed -n 's/^Version: *//p')
  '$BIN' secret rotate-key >/dev/null || exit 1
  depois=\$('$BIN' secret inspect sec-$PFX 2>&1 | sed -n 's/^Version: *//p')
  [ -n \"\$antes\" ] && [ \"\$depois\" = \"\$antes\" ] || { echo \"a versão mudou numa rotação de chave: \$antes -> \$depois\"; exit 1; }"

check "secret unset: a chave sai, o segredo fica" ok bash -c "
  '$BIN' secret unset sec-$PFX k >/dev/null || exit 1
  '$BIN' secret inspect sec-$PFX --reveal 2>&1 | grep -q 'k=' && { echo 'a chave ficou'; exit 1; }
  '$BIN' secret inspect sec-$PFX >/dev/null"
check "secret unset: uma chave de um segredo inexistente é 4, não 1" 4 "$BIN" secret unset nao-existe-$PFX k

# --- snapshots de volume: a listagem e o restore -----------------------------
check "volume snapshot ls: lista o snapshot que o create acabou de tirar" ok bash -c "
  '$BIN' volume create vs-$PFX >/dev/null 2>&1
  s=\$('$BIN' volume snapshot create vs-$PFX 2>&1 | sed -n \"s/.*snapshot '\\([^']*\\)'.*/\\1/p\")
  [ -n \"\$s\" ] || { echo 'o create não nomeou o snapshot'; exit 1; }
  '$BIN' volume snapshot ls vs-$PFX 2>&1 | grep -q \"\$s\" || { echo \"o ls não mostra \$s\"; exit 1; }
  echo \"\$s\" >'$OUT/vsnap-$PFX'"
check "volume snapshot restore: repõe os dados do snapshot" ok bash -c "
  s=\$(cat '$OUT/vsnap-$PFX')
  '$BIN' volume snapshot restore vs-$PFX \"\$s\" >/dev/null"
check "volume snapshot restore: um snapshot inexistente é 4, não 1" 4 "$BIN" volume snapshot restore vs-$PFX nao-existe

# --- os verbos de dia-2 de um workload/pod que não existe: classe 4 ----------
# A CLASSE é a parte que um reconciliador lê. Um `fail` continuaria verde se
# todas voltassem a colapsar em 1 (ver a secção dos códigos de saída).
check "workload stop: um nome inexistente é 4" 4 "$BIN" workload stop nao-existe-$PFX
check "workload rm: um nome inexistente é 4" 4 "$BIN" workload rm nao-existe-$PFX
check "pod logs: um pod inexistente é 4, e aponta para o get" 4 bash -c "
  out=\$('$BIN' pod logs nao-existe-$PFX 2>&1); rc=\$?
  grep -q 'get pods' <<<\"\$out\" || { echo 'não aponta para o get'; exit 99; }
  exit \$rc"

# A bateria limpa atrás de si, e aqui isso não é só arrumação: o root isolado é
# reutilizado entre corridas, e o `secret rotate-key` desta secção recifra TODOS
# os segredos do store — um `sec-<pfx>` deixado por corrida fazia a rotação da
# corrida seguinte pagar o lixo da anterior, e o custo cresceria sem ninguém dar
# por isso. O volume do snapshot sai pelo `volume prune` da secção das
# varreduras (está sem referência), mas é removido aqui pelo nome, para não
# depender da ordem de duas secções.
"$BIN" secret rm "sec-$PFX" >/dev/null 2>&1
"$BIN" volume rm "vs-$PFX" >/dev/null 2>&1
rm -f "$OUT/vsnap-$PFX" "$OUT/sec-$PFX.yaml" "$OUT/bad-manifest.yaml"
rm -rf "$OUT/ed-$PFX"

########################################
section "mais leituras e ciclos locais (F0.2, lote 2: container/image/vm/cluster)"
########################################
# Continuação da secção acima — a mesma disciplina (invariante real, nunca só
# o rc), sobre o grupo com mais folhas por folha-coberta: `container`.

# --- `container kill`: NÃO força Stopped, o próximo observe é que decide ----
check "container run para kill" ok "$BIN" container run -d --name "kc-$PFX" --net none "$IMG" sleep 600
check "container kill" ok "$BIN" container kill "kc-$PFX"
check "kill: o estado real acaba Crashed, nunca Stopped" ok bash -c "
  for _ in \$(seq 1 40); do
    s=\$('$BIN' container inspect 'kc-$PFX' | python3 -c 'import json,sys; print(json.load(sys.stdin)[0][\"status\"])')
    [ \"\$s\" = Crashed ] && exit 0
    [ \"\$s\" = Stopped ] && { echo 'kill forçou Stopped'; exit 1; }
    sleep 0.5
  done
  echo \"nunca saiu de '\$s'\"; exit 1"
e2e_rm "kc-$PFX"

# --- `container pause`/`unpause`: o freezer do cgroup v2, não um proxy -------
check "container run para pause" ok "$BIN" container run -d --name "pc-$PFX" --net none "$IMG" sleep 600
PCPID=$("$BIN" container inspect "pc-$PFX" | python3 -c 'import json,sys; print(json.load(sys.stdin)[0]["pid"])' 2>/dev/null)
PCCG=$(sed -n 's#^0::##p' "/proc/$PCPID/cgroup" 2>/dev/null)
check "container pause" ok "$BIN" container pause "pc-$PFX"
check "pause: cgroup.freeze diz 1" ok bash -c "[ \"\$(cat '/sys/fs/cgroup$PCCG/cgroup.freeze' 2>/dev/null)\" = 1 ]"
check "container unpause" ok "$BIN" container unpause "pc-$PFX"
check "unpause: cgroup.freeze volta a 0" ok bash -c "[ \"\$(cat '/sys/fs/cgroup$PCCG/cgroup.freeze' 2>/dev/null)\" = 0 ]"
e2e_rm "pc-$PFX"

# --- `container restart`: reaproveita o rootfs, ganha um pid NOVO -----------
check "container run para restart" ok "$BIN" container run -d --name "rsc-$PFX" --net none "$IMG" sleep 600
RSCPID1=$("$BIN" container inspect "rsc-$PFX" | python3 -c 'import json,sys; print(json.load(sys.stdin)[0]["pid"])' 2>/dev/null)
check "container restart" ok "$BIN" container restart "rsc-$PFX"
check "restart: pid novo, status Running" ok bash -c "
  for _ in \$(seq 1 40); do
    out=\$('$BIN' container inspect 'rsc-$PFX' | python3 -c 'import json,sys; d=json.load(sys.stdin)[0]; print(d[\"pid\"],d[\"status\"])')
    set -- \$out
    [ \"\$2\" = Running ] && [ \"\$1\" != '$RSCPID1' ] && exit 0
    sleep 0.5
  done
  echo \"ficou em '\$out', pid antigo era $RSCPID1\"; exit 1"
e2e_rm "rsc-$PFX"

# --- `container diff`: A/D relativo à imagem ---------------------------------
check "container run para diff" ok "$BIN" container run -d --name "dc-$PFX" --net none "$IMG" sleep 600
check "um exec que cria e apaga um ficheiro" ok "$BIN" container exec "dc-$PFX" sh -c "echo hi > /novo-$PFX.txt; rm -f /etc/motd"
check "container diff: mostra o A e o D" ok bash -c "
  out=\$('$BIN' container diff 'dc-$PFX') || exit 1
  grep -q \"A /novo-$PFX.txt\" <<<\"\$out\" || { echo 'não mostrou o A do ficheiro criado'; exit 1; }
  grep -q 'D /etc/motd' <<<\"\$out\" || { echo 'não mostrou o D do ficheiro apagado'; exit 1; }"

# --- `container cp`: os dois sentidos, byte a byte ---------------------------
check "container cp: host -> container" ok bash -c "
  echo 'conteudo-$PFX' >'$OUT/cpin-$PFX.txt'
  '$BIN' container cp '$OUT/cpin-$PFX.txt' 'dc-$PFX:/cpin-$PFX.txt'"
check "container cp: container -> host, byte a byte" ok bash -c "
  '$BIN' container cp 'dc-$PFX:/cpin-$PFX.txt' '$OUT/cpout-$PFX.txt' || exit 1
  diff '$OUT/cpin-$PFX.txt' '$OUT/cpout-$PFX.txt'"
e2e_rm "dc-$PFX"
rm -f "$OUT/cpin-$PFX.txt" "$OUT/cpout-$PFX.txt"

# --- `container apply`: `kind: Container` directo, idempotente por nome -----
check "container apply: cria do manifesto" ok bash -c "
  printf 'apiVersion: compute.delonix.io/v1alpha1\nkind: Container\nmetadata:\n  name: ca-$PFX\nspec:\n  image: $IMG\n  network: none\n  command: [\"sleep\", \"600\"]\n' >'$OUT/ca-$PFX.yaml'
  '$BIN' container apply -f '$OUT/ca-$PFX.yaml'"
check "container apply: o container do manifesto está Running" ok bash -c "
  '$BIN' container inspect 'ca-$PFX' | python3 -c 'import json,sys; sys.exit(0 if json.load(sys.stdin)[0][\"status\"]==\"Running\" else 1)'"
e2e_rm "ca-$PFX"
rm -f "$OUT/ca-$PFX.yaml"

# --- `container init`: as duas peças, já preenchidas -------------------------
check "container init: escreve Delonixfile + manifesto numa pasta vazia" ok bash -c "
  d='$OUT/cinit-$PFX'; mkdir -p \"\$d\"
  '$BIN' container init \"\$d\" || exit 1
  [ -s \"\$d/Delonixfile\" ] || { echo 'Delonixfile vazio ou em falta'; exit 1; }
  [ -s \"\$d/delonix-manifest.yaml\" ] || { echo 'manifesto vazio ou em falta'; exit 1; }"
rm -rf "$OUT/cinit-$PFX"

# --- `image apply`: `kind: Image` com pull, idempotente ---------------------
check "image apply: puxa a imagem do manifesto" ok bash -c "
  printf 'apiVersion: artifact.delonix.io/v1alpha1\nkind: Image\nmetadata:\n  name: ia-$PFX\nspec:\n  pull: $IMG\n' >'$OUT/ia-$PFX.yaml'
  '$BIN' image apply -f '$OUT/ia-$PFX.yaml'"
check "image apply: a imagem fica listada" ok bash -c "
  '$BIN' image ls | grep -qF '$IMG'"
rm -f "$OUT/ia-$PFX.yaml"

# --- `image history`: camadas base-a-topo, digest + tamanho -----------------
check "image history: pelo menos uma camada com digest e tamanho" ok bash -c "
  out=\$('$BIN' image history '$IMG') || exit 1
  grep -qE 'sha256:[0-9a-f]+' <<<\"\$out\" || { echo 'sem digest'; exit 1; }"

# --- `dashboard --once --json`: um snapshot, sem TUI -------------------------
check "dashboard --once --json: JSON com scope e tiles" ok bash -c "
  '$BIN' dashboard --once --json | python3 -c 'import json,sys; d=json.load(sys.stdin); sys.exit(0 if d.get(\"scope\")==\"global\" and isinstance(d.get(\"tiles\"),list) and len(d[\"tiles\"])>0 else 1)'"

# --- `vm default-backend`: lê/escreve um ÚNICO valor no ficheiro de providers
check "vm default-backend: sem --set devolve o estado actual" ok bash -c "
  '$BIN' vm default-backend | grep -qE 'none \(auto|libvirt|cloud-hypervisor'"
check "vm default-backend --set libvirt" ok "$BIN" vm default-backend --set libvirt
check "vm default-backend: o get devolve o que o set escreveu" ok bash -c "
  [ \"\$('$BIN' vm default-backend)\" = libvirt ]"
check "vm default-backend --set <desconhecido>: recusa pelo nome, nomeia os aceites" fail bash -c "
  out=\$('$BIN' vm default-backend --set bogus-$PFX 2>&1); rc=\$?
  grep -q \"unknown VM backend: 'bogus-$PFX'\" <<<\"\$out\" || { echo 'recusou sem nomear o valor'; exit 99; }
  grep -q 'cloud-hypervisor' <<<\"\$out\" || { echo 'não diz os aceites'; exit 99; }
  exit \$rc"

# --- `cluster ls`: sem privilégio, sem cluster nenhum, rc 0 ------------------
check "cluster ls: sem clusters, ainda assim rc 0" ok "$BIN" cluster ls

########################################
section "batch 3 (S15/N11): leaves that only ever had their --help checked"
########################################
# Third batch of the F0.2 coverage work, with the discipline of the two above:
# every check asserts the invariant the leaf PROMISES (a field of its output,
# an exit CLASS, an effect on a file or on the dataplane), never only that it
# returned 0. A check that invokes a leaf and judges nothing raises the
# numerator of `scripts/cli_exec_ratchet.py` and tests nothing.
#
# All of it runs on this host with the two isolated roots: no hypervisor, no
# root, no remote node, no network beyond the image the battery already pulled.
# Where a leaf can only be reached for real with one of those, what runs here is
# the refusal it gives without them — judged by its class and by what it
# names, because a refusal that names nothing is the same failure as an
# acceptance that does nothing.
# Isolated roots only: these checks attach netns, publish ports, write hosts
# and snapshot files, and stand up a fake kind cluster — on the user's own state
# they would be measuring and touching the machine, not a run of this battery.
if [[ $E2E_ISOLATED -ne 1 || $E2E_HAVE_IMAGE -ne 1 ]]; then
  skip "batch 3 (S15/N11)" "needs the isolated roots and $IMG (see the header and the image section)"
else
L3="$OUT/l3-$PFX"; L3H="$L3/home"; mkdir -p "$L3" "$L3H/.kube"

# --- the top-level `plan`/`wait`/`diff`: the generic verbs over one manifest --
# `stack plan`/`stack wait` have their own checks; the top-level spellings were
# never run. The CYCLE is what proves them — each step alone returns 0 even over
# a verb that plans nothing — so they share one manifest: plan says Create,
# apply, wait returns once it is up, plan says nothing to do, and `diff` reads
# back the three values of one field before and after a change made by hand.
cat >"$L3/m.yaml" <<YAML
apiVersion: compute.delonix.io/v1alpha1
kind: Container
metadata:
  name: l3w-$PFX
spec:
  image: $IMG
  network: none
  memory: 64M
  command: ["sleep", "600"]
YAML
check "plan: before the apply there is something to create (2)" 2 "$BIN" plan -f "$L3/m.yaml" --detailed-exitcode
check "plan: names the Create and creates nothing" ok bash -c "
  '$BIN' plan -f '$L3/m.yaml' 2>&1 | grep -qE '^ +\+ +Container/l3w-$PFX' || { echo 'the plan does not name the resource'; exit 1; }
  '$BIN' container inspect 'l3w-$PFX' >/dev/null 2>&1; rc=\$?
  [ \$rc -eq 0 ] && { echo 'a plan created the container'; exit 1; }
  [ \$rc -eq 4 ] || { echo \"container inspect: rc=\$rc, not 4 (gone)\"; exit 1; }
  true"
check "stack apply of the plan/wait/diff manifest" ok "$BIN" stack apply -f "$L3/m.yaml"
check "wait: returns once the manifest's container is Running" ok bash -c "
  timeout 90 '$BIN' wait -f '$L3/m.yaml' --timeout 60 || exit 1
  '$BIN' container inspect 'l3w-$PFX' | python3 -c 'import json,sys; sys.exit(0 if json.load(sys.stdin)[0][\"status\"]==\"Running\" else 1)'"
check "plan: after the apply there is nothing to do (0)" 0 "$BIN" plan -f "$L3/m.yaml" --detailed-exitcode
check "diff: the three columns, memory 64M on all of them" ok bash -c "
  out=\$('$BIN' diff containers 'l3w-$PFX' -f '$L3/m.yaml' 2>&1) || exit 1
  grep -q 'LAST-APPLIED' <<<\"\$out\" || { echo 'no LAST-APPLIED column'; exit 1; }
  grep -qE '^memory +64M +64M +64M' <<<\"\$out\" || { echo 'memory is not 64M on the three sides'; printf '%s\n' \"\$out\"; exit 1; }"
check "diff --detailed-exitcode: unchanged is 0" 0 "$BIN" diff containers "l3w-$PFX" -f "$L3/m.yaml" --detailed-exitcode
"$BIN" container update --memory 128M "l3w-$PFX" >/dev/null 2>&1
check "diff --detailed-exitcode: a change made by hand is 2" 2 "$BIN" diff containers "l3w-$PFX" -f "$L3/m.yaml" --detailed-exitcode
check "diff: OBSERVED shows the hand-made value, DESIRED the manifest's" ok bash -c "
  '$BIN' diff containers 'l3w-$PFX' -f '$L3/m.yaml' 2>&1 | grep -qE '^memory +64M +64M +128M'"
check "diff: a name the manifest does not declare is refused (1), naming the file" ok bash -c "
  out=\$('$BIN' diff containers nao-declarado-$PFX -f '$L3/m.yaml' 2>&1); rc=\$?
  [ \$rc -eq 1 ] || { echo \"expected 1, got \$rc\"; exit 1; }
  grep -qF '$L3/m.yaml' <<<\"\$out\""
# A manifest whose resource never comes up: `wait` gives up with 124 at its own
# timeout, never 0 and never a hang.
sed "s/l3w-$PFX/l3never-$PFX/" "$L3/m.yaml" >"$L3/never.yaml"
check "wait: a resource that never appears times out with 124" 124 timeout -s KILL 60 "$BIN" wait -f "$L3/never.yaml" --timeout 3

# --- `cluster kube generate`: a `kind: Pod` out of the container above --------
check "cluster kube generate: a kind: Pod with the container's image and command" ok bash -c "
  out=\$('$BIN' cluster kube generate 'l3w-$PFX' 2>&1) || exit 1
  grep -qx 'kind: Pod' <<<\"\$out\" || { echo 'not a kind: Pod'; exit 1; }
  grep -qF 'image: $IMG' <<<\"\$out\" || { echo 'the image is missing'; exit 1; }
  grep -q 'sleep' <<<\"\$out\" || { echo 'the command is missing'; exit 1; }"
check "cluster kube generate: a name that is neither container nor pod is 4" 4 "$BIN" cluster kube generate "nao-existe-$PFX"

# --- `cluster network`: the three leaves the profile work added --------------
# Chosen so the battery EXERCISES them without a cluster and without reaching
# the network: `profiles` is pure, and the other two are asserted on their
# refusal paths. The alternative — a real two-node bootstrap — is the live-lab
# procedure in docs/network-compatibility.md, not something a battery run on an
# isolated root can do.
#
# `profiles` prints what THIS binary would install, so the assertions are
# against the table in `cmd::net_profiles`, not against a web page: every
# profile name the CLI accepts has a row, and an experimental one carries the
# reason it is experimental.
check "cluster network profiles: a row per profile the CLI accepts" ok bash -c "
  out=\$('$BIN' cluster network profiles 2>&1) || exit 1
  for p in cilium calico flannel; do
    grep -qw \"\$p\" <<<\"\$out\" || { echo \"profile \$p is missing from the table\"; exit 1; }
  done
  grep -qiE 'experimental' <<<\"\$out\" || { echo 'no support level is reported'; exit 1; }"
# The divergence contract: a status over a cluster that does not exist cannot
# report 0. `status` returning success with nothing to look at is exactly the
# dishonest reporting its own --help promises not to do.
check "cluster network status: a cluster that does not exist refuses" fail \
  "$BIN" cluster network status "nao-existe-$PFX"
# `fetch` verifies every artefact against the digest this release pins. An
# unknown profile has no pin, so it is refused BEFORE anything is downloaded —
# which is what makes this check cheap and offline.
check "cluster network fetch: an unknown profile refuses before downloading" fail \
  "$BIN" cluster network fetch --cni "nao-existe-$PFX"

# --- image: export, sbom, scan --sbom, remove ---------------------------------
check "image export: an OCI runtime bundle — config.json with the process, and the rootfs" ok bash -c "
  d='$L3/bundle'; rm -rf \"\$d\"
  '$BIN' image export '$IMG' \"\$d\" >/dev/null || exit 1
  python3 -c 'import json,sys; c=json.load(open(sys.argv[1])); sys.exit(0 if c.get(\"ociVersion\") and c[\"process\"][\"args\"] else 1)' \"\$d/config.json\" || { echo 'config.json without ociVersion/process.args'; exit 1; }
  [ -s \"\$d/rootfs/etc/alpine-release\" ] || { echo 'the rootfs is not the image'; exit 1; }"
rm -rf "$L3/bundle"
check "image sbom: an SPDX 2.3 document that lists the image's packages" ok bash -c "
  '$BIN' image sbom '$IMG' | python3 -c '
import json, sys
d = json.load(sys.stdin)
assert d[\"spdxVersion\"] == \"SPDX-2.3\", d.get(\"spdxVersion\")
names = {p[\"name\"] for p in d[\"packages\"]}
assert \"busybox\" in names, sorted(names)[:10]'"
check "image scan --sbom: the package table names busybox, from the CAS, without running anything" ok bash -c "
  '$BIN' image scan --sbom '$IMG' 2>&1 | grep -qw busybox"
# `image remove` of one of two names UNTAGS (the image and the other name stay);
# of the last name it deletes the record. A committed image is the subject:
# nothing runs from it, so the in-use guard does not get in the way, and the
# base image of the battery is never at stake.
check "image remove: of one of two names untags, the image keeps the other" ok bash -c "
  '$BIN' container commit 'l3w-$PFX' 'l3rm-$PFX:v1' >/dev/null || exit 1
  '$BIN' image tag 'l3rm-$PFX:v1' 'l3rm-$PFX:v2' >/dev/null || exit 1
  out=\$('$BIN' image remove 'l3rm-$PFX:v1' 2>&1) || { printf '%s\\n' \"\$out\"; exit 1; }
  grep -q 'untagged' <<<\"\$out\" || { echo \"not an untag: \$out\"; exit 1; }
  ls=\$('$BIN' image ls)
  grep -qF 'l3rm-$PFX:v1' <<<\"\$ls\" && { echo 'the removed name is still listed'; exit 1; }
  grep -qF 'l3rm-$PFX:v2' <<<\"\$ls\" || { echo 'removing one name took the other with it'; exit 1; }"
check "image remove: of the last name deletes the image" ok bash -c "
  '$BIN' image remove 'l3rm-$PFX:v2' 2>&1 | grep -q 'deleted' || exit 1
  '$BIN' image describe 'l3rm-$PFX:v2' >/dev/null 2>&1; [ \$? -eq 4 ]"
check "image remove: an image that is not in the store is 4" 4 "$BIN" image remove "nao-existe-$PFX:v1"

# --- image login/logout: the credential file, and nothing else ---------------
# `.invalid` never resolves (RFC 6761): login only writes `auth.json`, so
# nothing is contacted, and the value is a throwaway made up for this run.
L3REG="l3-$PFX.invalid"
check "image login without --password-stdin is refused (1) and names the flag" ok bash -c "
  out=\$('$BIN' image login '$L3REG' -u e2e </dev/null 2>&1); rc=\$?
  [ \$rc -eq 1 ] || { echo \"expected 1, got \$rc\"; exit 1; }
  grep -q -- '--password-stdin' <<<\"\$out\""
check "image login: stored in auth.json at 0600, and the secret is never echoed" ok bash -c "
  out=\$(printf 'tok-$PFX' | '$BIN' image login '$L3REG' -u e2e --password-stdin 2>&1) || exit 1
  grep -q 'tok-$PFX' <<<\"\$out\" && { echo 'the secret was echoed'; exit 1; }
  f=\"\$DELONIX_ROOT/auth.json\"
  [ \"\$(stat -c %a \"\$f\")\" = 600 ] || { echo \"auth.json is \$(stat -c %a \"\$f\")\"; exit 1; }
  python3 -c 'import json,sys; sys.exit(0 if sys.argv[2] in json.load(open(sys.argv[1]))[\"auths\"] else 1)' \"\$f\" '$L3REG'"
check "image logout: the registry's entry leaves auth.json" ok bash -c "
  '$BIN' image logout '$L3REG' >/dev/null || exit 1
  python3 -c 'import json,sys; sys.exit(1 if sys.argv[2] in json.load(open(sys.argv[1])).get(\"auths\",{}) else 0)' \"\$DELONIX_ROOT/auth.json\" '$L3REG'"

# --- image verify: the key is parsed before any registry is asked ------------
# A key that is not a PEM public key is the caller's mistake, and it is refused
# as such (1) — not reported as "image not signed" after a trip to the registry.
check "image verify: a key that is not a PEM public key is refused (1) before the registry" ok bash -c "
  printf 'not a pem\\n' >'$L3/bad.pem'
  out=\$('$BIN' image verify '$IMG' '$L3/bad.pem' 2>&1); rc=\$?
  [ \$rc -eq 1 ] || { echo \"expected 1, got \$rc\"; exit 1; }
  grep -q 'invalid PEM public key' <<<\"\$out\""

# --- net tunnel: a token that would be an OPTION of the provider's binary -----
# The CRITICAL of the second audit round: pinggy's token is the LAST positional
# argument of `ssh`, and `-oProxyCommand=<cmd>` there is parsed as an option —
# local code execution before any connection. The proof is not the refusal
# message, it is that the file the payload would create does NOT exist, and
# that no tunnel record was written. Nothing is contacted: the refusal comes
# before any process is spawned.
L3PWN="$L3/pwned"
check "net tunnel expose: a token starting with '-' is refused (1), and the payload never ran" ok bash -c "
  out=\$('$BIN' net tunnel expose 9 --name l3t-$PFX --token='-oProxyCommand=touch $L3PWN' 2>&1); rc=\$?
  [ \$rc -eq 1 ] || { echo \"expected 1, got \$rc\"; exit 1; }
  grep -q \"cannot start with '-'\" <<<\"\$out\" || { echo 'refused without saying why'; exit 1; }
  [ ! -e '$L3PWN' ] || { echo 'THE PAYLOAD RAN'; exit 1; }
  '$BIN' get gateways 2>&1 | grep -q 'l3t-$PFX' && { echo 'a tunnel record was left behind'; exit 1; }
  true"
cat >"$L3/tunnel.yaml" <<YAML
apiVersion: gateway.delonix.io/v1alpha1
kind: Gateway
metadata:
  name: l3ta-$PFX
spec:
  provider: pinggy
  localPort: 9
  token: "-oProxyCommand=touch $L3PWN"
YAML
check "net tunnel apply: the same token in a manifest is refused, and the payload never ran" ok bash -c "
  out=\$('$BIN' net tunnel apply -f '$L3/tunnel.yaml' 2>&1); rc=\$?
  [ \$rc -eq 1 ] || { echo \"expected 1, got \$rc\"; exit 1; }
  grep -q \"cannot start with '-'\" <<<\"\$out\" || { echo 'refused without saying why'; exit 1; }
  [ ! -e '$L3PWN' ] || { echo 'THE PAYLOAD RAN'; exit 1; }"

# --- network vlan: a DRY-RUN by default, and `--apply` without root refused --
# The one command of `network` that needs root. A NIC that does not exist on
# this host keeps every branch harmless even if a regression ever made the
# dry-run apply something.
L3NIC="l3nic$$"
check "network vlan: the dry-run prints the two ip(8) steps and says it changed nothing" ok bash -c "
  out=\$('$BIN' network vlan '$L3NIC' 100 2>&1) || exit 1
  grep -q 'DRY-RUN' <<<\"\$out\" || { echo 'not announced as a dry-run'; exit 1; }
  grep -qF 'ip link add link $L3NIC name $L3NIC.100 type vlan id 100' <<<\"\$out\" || { echo 'no create step'; exit 1; }
  grep -qF 'ip link set $L3NIC.100 up' <<<\"\$out\" || { echo 'no up step'; exit 1; }"
check "network vlan: an id outside 1-4094 is refused (1) by name" ok bash -c "
  out=\$('$BIN' network vlan '$L3NIC' 4095 2>&1); rc=\$?
  [ \$rc -eq 1 ] && grep -q '1-4094' <<<\"\$out\""
if [ "$(id -u)" -ne 0 ]; then
  check "network vlan --apply without root is refused (1) before touching the host" ok bash -c "
    out=\$('$BIN' network vlan '$L3NIC' 100 --apply 2>&1); rc=\$?
    [ \$rc -eq 1 ] || { echo \"expected 1, got \$rc\"; exit 1; }
    grep -q 'needs root' <<<\"\$out\""
else
  skip "network vlan --apply without root" "this run IS root"
fi

# --- system: thermal, regulate, snapshot ---------------------------------------
# `system thermal` writes `cpu.max` on the DELEGATED BASE, which is the user's
# and not this run's (a cgroup is not per state root) — so the governor itself
# is not run here. What runs is the refusal that has to come before it writes
# anything: a `--high` at or below `--low` would never cool and never restore.
check "system thermal: --high at or below --low is refused (1) before any write" ok bash -c "
  out=\$('$BIN' system thermal --high 70 --low 80 --once 2>&1); rc=\$?
  [ \$rc -eq 1 ] || { echo \"expected 1, got \$rc\"; exit 1; }
  grep -q -- '--high must be greater than --low' <<<\"\$out\""
# Without `--apply`, `regulate` is a REPORT: whatever it plans, it writes no
# `cpu.weight`. Measured with a running container of this run, before and
# after; on a host under load the plan may well propose throttling it.
check "system regulate (no --apply): a report — applied false, cpu.weight untouched" ok bash -c "
  pid=\$('$BIN' container inspect 'l3w-$PFX' | python3 -c 'import json,sys; print(json.load(sys.stdin)[0][\"pid\"])')
  cg=\$(sed -n 's#^0::##p' /proc/\$pid/cgroup)
  w0=\$(cat /sys/fs/cgroup\$cg/cpu.weight 2>/dev/null)
  '$BIN' system regulate --once -o json | python3 -c '
import json, sys
d = json.load(sys.stdin)
assert d[\"applied\"] is False, d[\"applied\"]
assert all(a[\"applied\"] is False for a in d[\"actions\"]), d[\"actions\"]
assert any(w[\"name\"] == sys.argv[1] for w in d[\"workloads\"]), [w[\"name\"] for w in d[\"workloads\"]]' 'l3w-$PFX' || exit 1
  [ \"\$(cat /sys/fs/cgroup\$cg/cpu.weight 2>/dev/null)\" = \"\$w0\" ] || { echo \"cpu.weight moved: \$w0 -> \$(cat /sys/fs/cgroup\$cg/cpu.weight)\"; exit 1; }"
# `system snapshot create` packs what cannot be rebuilt and NEVER the live
# plumbing of a running node: a resurrected `holder.pid` makes the engine report
# a holder that does not exist. The archive is created 0600 (it carries the
# secrets). `system snapshot restore` until now was counted as executed only
# through a TAB-completion probe, which never runs the leaf.
check "system snapshot create: a 0600 archive with the registries and without the live plumbing" ok bash -c "
  a='$L3/node.tar.gz'; rm -f \"\$a\"
  '$BIN' system snapshot create -o \"\$a\" >/dev/null || exit 1
  [ \"\$(stat -c %a \"\$a\")\" = 600 ] || { echo \"the archive is \$(stat -c %a \"\$a\")\"; exit 1; }
  ls=\$(tar -tzf \"\$a\") || exit 1
  grep -q 'state/containers/' <<<\"\$ls\" || { echo 'the container registry is not in it'; exit 1; }
  grep -q 'holder.pid' <<<\"\$ls\" && { echo 'the live holder.pid travelled'; exit 1; }
  true"
# A secret written AFTER the snapshot is what a restore would take away. With a
# workload of this run still up, the restore refuses and names it; with
# `--force --dry-run` it names the secret as an entry to remove — and the secret
# is still there afterwards, because a dry-run changes nothing.
"$BIN" secret set "l3snap-$PFX" k=v >/dev/null 2>&1
check "system snapshot restore: refused (1) while a workload runs, naming it" ok bash -c "
  out=\$('$BIN' system snapshot restore '$L3/node.tar.gz' 2>&1); rc=\$?
  [ \$rc -eq 1 ] || { echo \"expected 1, got \$rc\"; exit 1; }
  grep -q 'l3w-$PFX' <<<\"\$out\" || { echo 'refused without naming the running workload'; exit 1; }
  '$BIN' secret inspect 'l3snap-$PFX' >/dev/null"
check "system snapshot restore --dry-run: names what it would remove, and removes nothing" ok bash -c "
  out=\$('$BIN' system snapshot restore '$L3/node.tar.gz' --dry-run --force 2>&1) || exit 1
  grep -q 'secrets/l3snap-$PFX.json' <<<\"\$out\" || { echo 'the secret written after the snapshot is not in the plan'; printf '%s\\n' \"\$out\"; exit 1; }
  grep -q 'nothing was changed' <<<\"\$out\" || { echo 'not announced as a dry-run'; exit 1; }
  '$BIN' secret inspect 'l3snap-$PFX' >/dev/null || { echo 'the dry-run removed the secret'; exit 1; }"
"$BIN" secret rm "l3snap-$PFX" >/dev/null 2>&1

# --- network node init/key: the overlay node key ------------------------------
# Idempotent, the private half at 0600, and the public key printed is the one
# `wg` derives from the private key on disk — not a second key.
if command -v wg >/dev/null 2>&1; then
  check "network node init: prints the public key and keeps the private one at 0600" ok bash -c "
    out=\$('$BIN' network node init 2>&1) || exit 1
    k=\"\$DELONIX_ROOT/wg/node.key\"
    [ \"\$(stat -c %a \"\$k\")\" = 600 ] || { echo \"node.key is \$(stat -c %a \"\$k\")\"; exit 1; }
    grep -qF \"\$(wg pubkey <\"\$k\")\" <<<\"\$out\" || { echo 'the printed key is not the one on disk'; exit 1; }"
  check "network node key: the same public key, and init again does not rotate it" ok bash -c "
    a=\$('$BIN' network node key) || exit 1
    '$BIN' network node init >/dev/null || exit 1
    b=\$('$BIN' network node key) || exit 1
    [ \"\$a\" = \"\$b\" ] && [ \"\$a\" = \"\$(wg pubkey <\"\$DELONIX_ROOT/wg/node.key\")\" ]"
else
  skip "network node init/key" "wg is not installed — the refusal path is checked in the network section"
fi

# --- vm day-2 verbs on a VM that does not exist: class 4, before any backend --
# The class is the part a reconciler reads, and these verbs resolve the record
# before they ask a hypervisor anything — so the answer is the same on a host
# with none installed.
check "vm destroy: a VM that does not exist is 4" 4 "$BIN" vm destroy "l3vm-$PFX"
check "vm restart: a VM that does not exist is 4" 4 "$BIN" vm restart "l3vm-$PFX"
check "vm update: a VM that does not exist is 4" 4 "$BIN" vm update "l3vm-$PFX" --vcpus 2
check "vm vnc: a VM that does not exist is 4" 4 "$BIN" vm vnc "l3vm-$PFX"
check "vm console: a VM that does not exist is 4" 4 "$BIN" vm console "l3vm-$PFX"
check "vm ssh: a name that is not a VM is 4" 4 "$BIN" vm ssh "l3vm-$PFX" -- true
check "vm migrate: a VM that does not exist is 4, before any SSH" 4 "$BIN" vm migrate "l3vm-$PFX" --host e2e@l3.invalid --network l3

# --- vm create on a network nobody declared: 4, before anything is built ----
# Measured 2026-10-21: libvirt built the overlay, defined the domain and died on
# its own «no network with matching name» (rc 1), and Cloud Hypervisor CREATED
# the network instead. The engine now asks first, so no hypervisor is reached;
# it still needs a backend to select and a disk to name.
if command -v qemu-img >/dev/null 2>&1 \
   && { command -v cloud-hypervisor || command -v virsh; } >/dev/null 2>&1; then
  UNDIR="$OUT/vm-undeclared-net"; mkdir -p "$UNDIR"
  qemu-img create -f qcow2 "$UNDIR/d.qcow2" 64M >/dev/null 2>&1
  check "vm create: a network nobody declared is 4 (DX-4505), and nothing is left behind" ok bash -c "
    out=\$(\"$BIN\" vm create un-$PFX --disk \"$UNDIR/d.qcow2\" --network un-nope-$PFX 2>&1); rc=\$?
    [ \$rc -eq 4 ] || { echo \"rc=\$rc: \$out\"; exit 1; }
    case \"\$out\" in *DX-4505*'delonix network create un-nope-$PFX'*) ;; *) echo \"\$out\"; exit 1;; esac
    ls \"\$DELONIX_ROOT/vms\" 2>/dev/null | grep -q 'un-$PFX' && { echo 'a VM artifact was left'; exit 1; }
    nl=\$(\"$BIN\" network ls 2>&1) || { echo \"network ls failed: \$nl\"; exit 1; }
    case \"\$nl\" in *'un-nope-$PFX'*) echo 'the network was created'; exit 1;; esac
    exit 0"
else
  skip "vm create: undeclared network" "no qemu-img, or no VM backend (cloud-hypervisor/virsh) to select"
fi

# --- mcp serve: one stdio session, and it ENDS with its client ---------------
# The server is a child process of the AI client for one session, not a daemon:
# it answers `initialize` and `tools/list` and exits when stdin closes. A
# `timeout` that fires (124) would be the server outliving its client.
check "mcp serve: answers initialize and tools/list over stdio, and exits at EOF" ok bash -c "
  out=\$(printf '%s\n' \
    '{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2024-11-05\",\"capabilities\":{},\"clientInfo\":{\"name\":\"e2e\",\"version\":\"0\"}}}' \
    '{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}' \
    '{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\"}' \
    | timeout 30 '$BIN' mcp serve 2>/dev/null); rc=\$?
  [ \$rc -eq 0 ] || { echo \"the server did not end cleanly with its client (rc=\$rc)\"; exit 1; }
  python3 -c '
import json, sys
resp = {m[\"id\"]: m for m in map(json.loads, filter(None, sys.stdin.read().splitlines())) if \"id\" in m}
assert \"serverInfo\" in resp[1][\"result\"], resp.get(1)
names = [t[\"name\"] for t in resp[2][\"result\"][\"tools\"]]
assert \"logs.query\" in names, names' <<<\"\$out\""

# --- cluster apply/upgrade: shell injection refused before any SSH ------------
# The CRITICAL of the first audit: `controlPlaneEndpoint`/`k8sVersion` were
# interpolated into the body of a remote `sudo bash -c`. The host below is a
# TEST-NET-1 address (RFC 5737, never routed), so a regression would show as an
# SSH timeout and not as a command on anybody's machine — and the payload's
# marker file is the proof that nothing ran here either.
cat >"$L3/cluster-ssh.yaml" <<YAML
apiVersion: infrastructure.delonix.io/v1alpha1
kind: KubernetesCluster
metadata:
  name: l3k-$PFX
spec:
  mode: ssh
  controlPlaneEndpoint: "192.0.2.10; touch $L3PWN; #"
  controlPlane:
    hosts:
      - address: 192.0.2.10
YAML
check "cluster apply: a shell payload in controlPlaneEndpoint is refused (1) and names the field" ok bash -c "
  out=\$(HOME='$L3H' KUBECONFIG='$L3H/.kube/config' timeout -s KILL 60 '$BIN' cluster apply -f '$L3/cluster-ssh.yaml' 2>&1); rc=\$?
  [ \$rc -eq 1 ] || { echo \"expected 1, got \$rc\"; printf '%s\n' \"\$out\"; exit 1; }
  grep -q 'controlPlaneEndpoint' <<<\"\$out\" || { echo 'refused without naming the field'; exit 1; }
  [ ! -e '$L3PWN' ] || { echo 'THE PAYLOAD RAN'; exit 1; }"
check "cluster upgrade: a shell payload in --to is refused (1) before the manifest is read" ok bash -c "
  out=\$(HOME='$L3H' KUBECONFIG='$L3H/.kube/config' '$BIN' cluster upgrade --to '1.32; touch $L3PWN' -f '$L3/nao-existe.yaml' 2>&1); rc=\$?
  [ \$rc -eq 1 ] || { echo \"expected 1, got \$rc\"; exit 1; }
  grep -q 'invalid Kubernetes version' <<<\"\$out\" || { echo 'refused for another reason'; printf '%s\n' \"\$out\"; exit 1; }
  [ ! -e '$L3PWN' ]"

# --- cluster init, and the verbs that need a cluster this engine knows -------
# `init` writes one manifest per mode, each valid on its own. drain/uncordon go
# to the apiserver through a CACHED kubeconfig and backup/restore through SSH to
# a `cluster kubeadm` control-plane: with none of those here, what is judged is
# that each refuses BEFORE reaching kubectl or ssh, and says what is missing.
check "cluster init: one manifest per mode (kind, vm, ssh), each one valid" ok bash -c "
  d='$L3/cinit'; rm -rf \"\$d\"
  HOME='$L3H' '$BIN' cluster init \"\$d\" >/dev/null || exit 1
  for m in kind vm ssh; do
    f=\"\$d/cluster-\$m.yaml\"
    grep -qE \"^ *mode: \$m\\\$\" \"\$f\" || { echo \"cluster-\$m.yaml does not say mode: \$m\"; exit 1; }
    '$BIN' manifest validate -f \"\$f\" >/dev/null 2>&1 || { echo \"cluster-\$m.yaml does not validate\"; exit 1; }
  done"
check "cluster drain: no cached cluster is refused (1) before kubectl, naming how to get one" ok bash -c "
  out=\$(HOME='$L3H' KUBECONFIG='$L3H/.kube/config' '$BIN' cluster drain l3node 2>&1); rc=\$?
  [ \$rc -eq 1 ] && grep -q 'no cluster kubeconfig cached' <<<\"\$out\""
check "cluster uncordon: a cluster name with no kubeconfig is refused (1), naming where it looked" ok bash -c "
  out=\$(HOME='$L3H' KUBECONFIG='$L3H/.kube/config' '$BIN' cluster uncordon l3node --name 'nao-existe-$PFX' 2>&1); rc=\$?
  [ \$rc -eq 1 ] && grep -qF 'nao-existe-$PFX-kubeconfig.yaml' <<<\"\$out\""
check "cluster backup: a cluster with no kubeadm control-plane is refused (1) before any SSH" ok bash -c "
  out=\$(HOME='$L3H' '$BIN' cluster backup 'nao-existe-$PFX' 2>&1); rc=\$?
  [ \$rc -eq 1 ] && grep -q 'only support clusters created by .cluster kubeadm.' <<<\"\$out\""
check "cluster restore: the same refusal, before the snapshot file is even read" ok bash -c "
  out=\$(HOME='$L3H' '$BIN' cluster restore 'nao-existe-$PFX' --from '$L3/nao-existe.db' 2>&1); rc=\$?
  [ \$rc -eq 1 ] && grep -q 'no VM-provisioned nodes' <<<\"\$out\""

# --- cluster stop/start/load/health/destroy over a stand-in kind cluster -------
# A kind-mode cluster IS its labelled containers (`cluster ls` derives it from
# `io.x-k8s.kind.cluster`, there is no other record). Two plain, unprivileged
# containers carrying the two labels are therefore a cluster to these verbs,
# with no kindest/node and no Kubernetes inside — enough to prove what each verb
# does to the NODES, which is the part it owns. HOME and KUBECONFIG point inside
# this run: `destroy` edits `~/.kube/config`, and a cluster name that misses the
# engine's cache falls back to `~/.kube/config-<name>`.
L3K="l3kc-$PFX"
for _r in control-plane worker; do
  "$BIN" container run -d --name "$L3K-$_r" --net none \
    --label "io.x-k8s.kind.cluster=$L3K" --label "io.x-k8s.kind.role=$_r" \
    "$IMG" sleep 600 >/dev/null 2>&1
done
# The status of both nodes, one line: `<control-plane> <worker>`.
L3KST="for r in control-plane worker; do '$BIN' container inspect '$L3K-'\$r | python3 -c 'import json,sys; print(json.load(sys.stdin)[0][\"status\"])'; done | xargs"
# A node whose exit waits on the host's disk writeback makes `stop` give up with
# DX-8101 and keep it recorded as running (ADR-0056 D4); the error says to run
# it again once the process is gone, and that is what the loop does. What is
# judged is the state of both nodes at the end.
check "cluster stop: every node of the cluster stops" ok bash -c "
  for _ in \$(seq 1 10); do
    HOME='$L3H' KUBECONFIG='$L3H/.kube/config' '$BIN' cluster stop '$L3K' >/dev/null 2>&1 && break
    sleep 2
  done
  s=\$($L3KST); case \"\$s\" in *Running*) echo \"a node still runs: \$s\"; exit 1;; esac"
check "cluster load: a cluster with no running node is refused (1), naming it" ok bash -c "
  out=\$(HOME='$L3H' KUBECONFIG='$L3H/.kube/config' '$BIN' cluster load '$IMG' --name '$L3K' 2>&1); rc=\$?
  [ \$rc -eq 1 ] && grep -q \"cluster '$L3K' has no running node\" <<<\"\$out\""
check "cluster start: every node of the cluster runs again" ok bash -c "
  HOME='$L3H' KUBECONFIG='$L3H/.kube/config' '$BIN' cluster start '$L3K' >/dev/null || exit 1
  s=\$($L3KST); [ \"\$s\" = 'Running Running' ] || { echo \"nodes are: \$s\"; exit 1; }"
# No apiserver behind these nodes: `health` must say so with a non-zero exit.
# A `health` returning 0 here is the dishonest report its own help says it
# never gives.
check "cluster health: a control-plane that does not answer /readyz is not healthy" ok bash -c "
  out=\$(HOME='$L3H' KUBECONFIG='$L3H/.kube/config' timeout -s KILL 90 '$BIN' cluster health '$L3K' 2>&1); rc=\$?
  [ \$rc -ne 0 ] || { echo 'health returned 0 over a cluster with no apiserver'; exit 1; }
  grep -q 'readyz' <<<\"\$out\" || { echo 'failed without naming the probe'; printf '%s\\n' \"\$out\"; exit 1; }"
check "cluster health: a cluster that does not exist is 4" 4 \
  env HOME="$L3H" KUBECONFIG="$L3H/.kube/config" "$BIN" cluster health "nao-existe-$PFX"
check "cluster destroy: the nodes go, and cluster ls no longer lists it" ok bash -c "
  for _ in \$(seq 1 10); do
    HOME='$L3H' KUBECONFIG='$L3H/.kube/config' '$BIN' cluster destroy '$L3K' --force >/dev/null 2>&1 && break
    sleep 2
  done
  '$BIN' container inspect '$L3K-control-plane' >/dev/null 2>&1; rc=\$?
  [ \$rc -eq 0 ] && { echo 'a node survived'; exit 1; }
  [ \$rc -eq 4 ] || { echo \"container inspect: rc=\$rc, not 4 (gone)\"; exit 1; }
  '$BIN' cluster ls 2>&1 | grep -q '$L3K' && { echo 'cluster ls still lists it'; exit 1; }
  true"
check "cluster destroy: a cluster that does not exist is 4" 4 \
  env HOME="$L3H" KUBECONFIG="$L3H/.kube/config" "$BIN" cluster destroy "nao-existe-$PFX" --force
for _r in control-plane worker; do e2e_rm "$L3K-$_r"; done

# --- systemcontainer snapshot restore/rm: the record is read before the node --
check "systemcontainer snapshot restore of a name not registered is 4" 4 \
  env DELONIX_PROVIDERS_CONFIG="$ADR54_EMPTY" "$BIN" systemcontainer snapshot restore "l3sc-$PFX" s1
check "systemcontainer snapshot rm of a name not registered is 4" 4 \
  env DELONIX_PROVIDERS_CONFIG="$ADR54_EMPTY" "$BIN" systemcontainer snapshot rm "l3sc-$PFX" s1

# --- net: the per-network egress policy, and the per-container clear -----------
L3N="l3n-$PFX"; L3C="l3c-$PFX"
if check_then "net egress: the network is created" ok "$BIN" network create "$L3N" \
   && check_then "net egress: its container starts" ok \
        "$BIN" container run -d --name "$L3C" --net "$L3N" "$IMG" sleep 600; then
  check "net egress show: a fresh network has no restriction and no FQDN allowlist" ok bash -c "
    out=\$('$BIN' net egress show '$L3N' 2>&1) || exit 1
    grep -q 'policy: allow' <<<\"\$out\" && grep -q 'FQDN allowlist: (none)' <<<\"\$out\""
  check "net egress host: the hostname enters the network's FQDN allowlist" ok bash -c "
    '$BIN' net egress host '$L3N' example.org >/dev/null || exit 1
    '$BIN' net egress show '$L3N' 2>&1 | grep -q 'example.org  (and \*.example.org)'"
  check "net egress net deny: the network's policy reads deny" ok bash -c "
    '$BIN' net egress net '$L3N' deny >/dev/null || exit 1
    '$BIN' net egress show '$L3N' 2>&1 | grep -qi 'policy: deny'"
  check "net egress net allowlist without --to is refused (1)" 1 "$BIN" net egress net "$L3N" allowlist
  check "net egress net allow: the policy is back to allow" ok bash -c "
    '$BIN' net egress net '$L3N' allow >/dev/null || exit 1
    '$BIN' net egress show '$L3N' 2>&1 | grep -qi 'policy: allow'"
  check "net egress show: a network that does not exist is 4" 4 "$BIN" net egress show "nao-existe-$PFX"
  # Per-container: policy + clear. `clear` takes the RULES and leaves the
  # direction's default standing — the same contract the `net ingress clear`
  # check of the delete-networkpolicies section holds for the other direction.
  check "net egress policy deny: the container's outbound default reads deny" ok bash -c "
    '$BIN' net egress policy '$L3C' deny >/dev/null || exit 1
    '$BIN' get networkpolicies 2>&1 | grep -qE '^$L3C +egress +deny'"
  check "net egress clear: the rules go and the default stays deny" ok bash -c "
    '$BIN' net egress deny '$L3C' 443 >/dev/null || exit 1
    '$BIN' net egress clear '$L3C' >/dev/null || exit 1
    '$BIN' net egress ls '$L3C' 2>&1 | grep -qE '443' && { echo 'a rule survived the clear'; exit 1; }
    '$BIN' get networkpolicies 2>&1 | grep -qE '^$L3C +egress +deny'"
fi

# --- net httproute apply/rm, and `hosts sync` over the names a route asks for --
# The L7 proxy end to end: a backend on the network above, a route with a Host
# rule and `hosts: [host]`, a request through the published entrypoint, then
# the teardown. `DELONIX_HOSTS_FILE` points every write at a file of this run:
# a route that asks for names makes the apply itself write the hosts file, and
# /etc/hosts is the user's. The backend is busybox `nc` in a loop — alpine's
# busybox has no `httpd`.
L3HR="hr-$PFX.e2e.test"; e2e_port L3HP; L3HF="$L3/hosts"
# The body's length follows $PFX (the run's pid): a fixed Content-Length cut the
# answer, and the proxy was right to honour it.
L3PONG="pong-$PFX"
printf '127.0.0.1 localhost\n' >"$L3HF"
if check_then "net httproute: the backend container starts" ok "$BIN" container run -d --name "$L3C-web" --net "$L3N" "$IMG" sh -c \
     "while true; do printf 'HTTP/1.0 200 OK\r\nContent-Length: ${#L3PONG}\r\n\r\n$L3PONG' | nc -l -p 80; done"; then
  cat >"$L3/route.yaml" <<YAML
apiVersion: gateway.delonix.io/v1alpha1
kind: HTTPRoute
metadata:
  name: l3hr-$PFX
spec:
  entrypoints:
    - { port: $L3HP }
  hosts: [host]
  rules:
    - host: $L3HR
      paths:
        - path: /
          backend: { service: $L3C-web, port: 80 }
YAML
  check "net httproute apply: the proxy serves the backend by the Host header" ok bash -c "
    DELONIX_HOSTS_FILE='$L3HF' '$BIN' net httproute apply -f '$L3/route.yaml' >/dev/null || exit 1
    for _ in \$(seq 1 40); do
      r=\$(curl -fsS --max-time 3 -H 'Host: $L3HR' http://127.0.0.1:$L3HP/ 2>/dev/null)
      [ \"\$r\" = pong-$PFX ] && exit 0
      sleep 0.5
    done
    echo \"the route never answered (last: '\$r')\"; exit 1"
  check "net httproute apply: a request for another Host is not routed to the backend" ok bash -c "
    r=\$(curl -sS --max-time 3 -H 'Host: other-$PFX.e2e.test' http://127.0.0.1:$L3HP/ 2>/dev/null)
    [ \"\$r\" != pong-$PFX ]"
  check "hosts sync --print: the route's name, pointed at 127.0.0.1" ok bash -c "
    out=\$(DELONIX_HOSTS_FILE='$L3HF' '$BIN' hosts sync --print) || exit 1
    grep -qE '^127\\.0\\.0\\.1[[:space:]]+$L3HR\$' <<<\"\$out\" || { echo 'the block does not carry the name'; printf '%s\\n' \"\$out\"; exit 1; }"
  # The block deleted by hand (an edited /etc/hosts): `sync` puts it back and
  # keeps every line that was not its own.
  check "hosts sync: rewrites a block that was edited away, keeping the other lines" ok bash -c "
    printf '127.0.0.1 localhost\\n10.9.9.9 keep-me\\n' >'$L3HF'
    DELONIX_HOSTS_FILE='$L3HF' '$BIN' hosts sync >/dev/null || exit 1
    grep -qx '127.0.0.1 localhost' '$L3HF' && grep -qx '10.9.9.9 keep-me' '$L3HF' || { echo 'a line that was there is gone'; exit 1; }
    grep -q '^# BEGIN delonix' '$L3HF' && grep -qE '[[:space:]]$L3HR\$' '$L3HF'"
  check "net httproute rm: the proxy stops, the entrypoint closes, the name leaves the file" ok bash -c "
    DELONIX_HOSTS_FILE='$L3HF' '$BIN' net httproute rm >/dev/null || exit 1
    grep -q '$L3HR' '$L3HF' && { echo 'the route is gone but its name is still in the hosts file'; exit 1; }
    for _ in \$(seq 1 20); do
      curl -fsS --max-time 2 -H 'Host: $L3HR' http://127.0.0.1:$L3HP/ >/dev/null 2>&1 || exit 0
      sleep 0.5
    done
    echo 'the entrypoint still answers after rm'; exit 1"
fi
e2e_rm "$L3C-web"

# --- net netns: the holder's plumbing, judged on the wire --------------------
# attach gives a netns an address on delonix0, exec runs a HOST binary inside
# it, firewall and publish act on traffic, detach takes the netns away. Two
# netns and the host's own `socat`/`nc` are the whole fixture: A answers a
# token, B connects. The token and not the TCP handshake is the proof: a
# published port's slirp listener accepts a connection whether or not anything
# answers behind it.
L3A="l3a$PFX"; L3B="l3b$PFX"
check "net netns attach: each netns gets its own address on delonix0" ok bash -c "
  a=\$('$BIN' net netns attach '$L3A' 2>&1 | sed -n 's/.*→ \\([0-9.]*\\) on delonix0.*/\\1/p')
  b=\$('$BIN' net netns attach '$L3B' 2>&1 | sed -n 's/.*→ \\([0-9.]*\\) on delonix0.*/\\1/p')
  [ -n \"\$a\" ] && [ -n \"\$b\" ] && [ \"\$a\" != \"\$b\" ] || { echo \"addresses: '\$a' '\$b'\"; exit 1; }
  echo \"\$a\" >'$L3/a.ip'"
L3AIP=$(cat "$L3/a.ip" 2>/dev/null)
if [[ -n "$L3AIP" ]] && command -v socat >/dev/null 2>&1; then
  check "net netns exec: the address attach printed is the one inside the netns" ok bash -c "
    '$BIN' net netns exec '$L3A' ip -4 -o addr show 2>&1 | grep -qF ' $L3AIP/'"
  # A server in A that ends on its own (`timeout`), so nothing outlives the run.
  "$BIN" net netns exec "$L3A" timeout 180 socat TCP-LISTEN:7171,fork,reuseaddr "SYSTEM:echo pong-$PFX" >/dev/null 2>&1 &
  L3LPID=$!
  L3REACH="[ \"\$('$BIN' net netns exec '$L3B' nc -w 2 '$L3AIP' 7171 </dev/null 2>/dev/null)\" = pong-$PFX ]"
  check "net netns exec: B reaches A's listener (the baseline)" ok bash -c "
    for _ in \$(seq 1 30); do $L3REACH && exit 0; sleep 0.3; done; exit 1"
  check "net netns firewall: an inbound deny on A cuts B off" ok bash -c "
    '$BIN' net netns firewall '$L3A' --spec '{\"enabled\":true,\"policyIn\":\"deny\",\"policyOut\":\"allow\",\"rules\":[]}' >/dev/null || exit 1
    ! $L3REACH"
  check "net netns firewall --clear: B reaches A again" ok bash -c "
    '$BIN' net netns firewall '$L3A' --clear >/dev/null || exit 1
    for _ in \$(seq 1 10); do $L3REACH && exit 0; sleep 0.3; done; exit 1"
  e2e_port L3PP
  check "net netns publish: the host port forwards to A's listener" ok bash -c "
    '$BIN' net netns publish '$L3A' '$L3PP:7171' >/dev/null || exit 1
    for _ in \$(seq 1 20); do [ \"\$(nc -w 2 127.0.0.1 $L3PP </dev/null 2>/dev/null)\" = pong-$PFX ] && exit 0; sleep 0.3; done; exit 1"
  check "net netns unpublish: the host port stops answering" ok bash -c "
    '$BIN' net netns unpublish '$L3PP' >/dev/null || exit 1
    for _ in \$(seq 1 20); do [ \"\$(nc -w 1 127.0.0.1 $L3PP </dev/null 2>/dev/null)\" = pong-$PFX ] || exit 0; sleep 0.3; done; exit 1"
  # The engine waits on the `timeout` it spawned and does not forward a signal
  # to it: kill that child first (it forwards to socat), then the engine.
  kill $(ps -o pid= --ppid "$L3LPID" 2>/dev/null) "$L3LPID" 2>/dev/null; wait "$L3LPID" 2>/dev/null
  check "net netns detach: the netns is gone, exec into it fails" ok bash -c "
    '$BIN' net netns detach '$L3B' >/dev/null || exit 1
    ! '$BIN' net netns exec '$L3B' true >/dev/null 2>&1"
else
  skip "net netns exec/firewall/publish/unpublish/detach" "attach printed no address, or socat is not installed"
fi
"$BIN" net netns detach "$L3A" >/dev/null 2>&1
"$BIN" net netns detach "$L3B" >/dev/null 2>&1
rm -f "$L3/a.ip"

# --- net l4guard status / net flow: what the holder reports, read back ------
# The infra installs the L4 guard at boot (50 new connections/s and 200
# concurrent per source, `ensure_up`), so `status` has something real to read:
# the two meter rules with their drop counters.
check "net l4guard status: the boot guard is live, both meter rules with counters" ok bash -c "
  out=\$('$BIN' net l4guard status 2>&1) || exit 1
  grep -q '^l4guard: active' <<<\"\$out\" || { echo 'the guard is not reported active'; exit 1; }
  grep -q 'dlx_conn_rate' <<<\"\$out\" && grep -q 'dlx_conn_count' <<<\"\$out\" || { echo 'a meter rule is missing'; exit 1; }
  grep -qE 'packets, [0-9]+ bytes dropped' <<<\"\$out\" || { echo 'no drop counters'; exit 1; }"

# --- net l4guard set / clear: the rule appears, and it goes ------------------
# `set` is best-effort BY DESIGN: if the kernel rejects `meter`, `do_l4guard`'s
# `nft -c` pre-flight applies nothing and still returns 0. So the exit code
# proves nothing here, and the values have to be read back out of the ruleset
# the holder actually carries. Distinctive values, not the boot guard's 50/200,
# so a stale rule cannot pass for a new one — and the check above already
# proved `meter` works on this kernel, which makes a rule that does not appear
# a real failure rather than a documented degrade.
check "net l4guard set: the two meter rules appear with the values given" ok bash -c "
  '$BIN' net l4guard set 77 333 >/dev/null 2>&1 || exit 1
  out=\$('$BIN' net l4guard status 2>&1)
  grep -q '^l4guard: active' <<<\"\$out\" || { echo 'not active after set'; echo \"\$out\"; exit 1; }
  grep -q 'over 77/second' <<<\"\$out\" || { echo 'the rate rule is not the one set'; echo \"\$out\"; exit 1; }
  grep -q 'ct count over 333' <<<\"\$out\" || { echo 'the count rule is not the one set'; echo \"\$out\"; exit 1; }"
# The class of the refusal, not just that it refused: both rates are `u32`
# positionals, so a non-numeric one is a USAGE error (2) and clap rejects it
# before the engine is reached — which is why this is safe to assert against a
# shared holder. A collapse of this to 1 would mean the guard started taking
# the argument and failing later, with the ruleset already touched.
check "net l4guard set: a non-numeric rate is a usage error, not an engine call" 2 \
  "$BIN" net l4guard set abc 1
# The half that was actually broken: `clear` listed `fwdeny` while `set` wrote
# to `forward`, so it deleted nothing, returned 0 and printed «l4guard:
# cleared» with the guard still live and still dropping. Asserting the rules
# are GONE — not that the command exited 0 — is the entire point of this check.
check "net l4guard clear: both meter rules leave the ruleset" ok bash -c "
  '$BIN' net l4guard clear >/dev/null 2>&1 || exit 1
  out=\$('$BIN' net l4guard status 2>&1)
  grep -q 'dlx_conn_rate' <<<\"\$out\" && { echo 'the rate rule survived clear'; echo \"\$out\"; exit 1; }
  grep -q 'dlx_conn_count' <<<\"\$out\" && { echo 'the count rule survived clear'; echo \"\$out\"; exit 1; }
  grep -q 'l4guard: not active' <<<\"\$out\" || { echo 'cleared, but not reported inactive'; echo \"\$out\"; exit 1; }"
# Put the infra's own guard back. `ensure_up` installs 50/200 at boot, and this
# holder is shared by UID across state roots: leaving the ingress unprotected
# because a test ran here is not an acceptable side effect of measuring.
check "net l4guard set: the boot guard is restored to 50/200" ok bash -c "
  '$BIN' net l4guard set 50 200 >/dev/null 2>&1 || exit 1
  out=\$('$BIN' net l4guard status 2>&1)
  grep -q 'over 50/second' <<<\"\$out\" || { echo 'the boot guard rate did not come back'; echo \"\$out\"; exit 1; }
  grep -q 'ct count over 200' <<<\"\$out\" || { echo 'the boot guard cap did not come back'; echo \"\$out\"; exit 1; }"

# Rootless has no CAP_BPF, so `flow` says the eBPF datapath is off and falls
# back to the veth counters — a row per container on the SDN, with its address.
check "net flow: the veth fallback lists the container with its SDN address" ok bash -c "
  ip=\$('$BIN' container inspect '$L3C' | python3 -c 'import json,sys; print(json.load(sys.stdin)[0][\"ip\"])')
  '$BIN' net flow 2>&1 | grep -E \"^$L3C +\$ip \" >/dev/null || { '$BIN' net flow 2>&1 | tail -5; exit 1; }"

# --- net capture: tcpdump inside the container's netns ------------------------
# Two checks, because the one that stood here asked one capture two questions.
# It kept the first 2 frames (`-c 2`) and wanted the container's ICMP among
# them, while the first frames on a fresh bridge are often ICMPv6 Neighbor
# Solicitations, IGMP from the gateway or ARP: measured 2026-10-11, origin/main
# passed it 4 times in 10. Now `-c` is judged by the COUNT alone (any frames),
# and the container's own traffic by a larger capture that the pings keep fed
# until it stops — neither depends on which frame comes first.
#
# `--duration` is not checked here, and on purpose: it stops tcpdump with a
# SIGINT, and Ubuntu's tcpdump AppArmor profile only accepts signals from
# `unconfined` peers. From a normal shell it stops on time (measured 3.2 s for
# `--duration 3`); from a process under another AppArmor label the signal is
# denied and the capture runs on. A check would read the label of whoever runs
# the battery, not the engine.
if command -v tcpdump >/dev/null 2>&1; then
  # $1 = how many frames to capture, $2 = the pcap. Pings to the gateway run
  # until the capture exits, so it always has traffic to count.
  e2e_capture() {
    local ip gw cap
    ip=$("$BIN" container inspect "$L3C" | python3 -c 'import json,sys; print(json.load(sys.stdin)[0]["ip"])')
    gw=$(echo "$ip" | awk -F. '{print $1"."$2".0.1"}')
    ( timeout -s KILL 60 "$BIN" net capture "$L3C" -w - -c "$1" >"$2" 2>/dev/null ) & cap=$!
    while kill -0 "$cap" 2>/dev/null; do
      "$BIN" container exec "$L3C" ping -c1 -W1 "$gw" >/dev/null 2>&1; sleep 0.2
    done
    wait "$cap"
    echo "$ip $gw"
  }
  cat >"$L3/pcap.py" <<'PY'
import socket, struct, sys
b = open(sys.argv[1], 'rb').read()
assert b[:4] in (b'\xd4\xc3\xb2\xa1', b'\xa1\xb2\xc3\xd4'), ('not a pcap', b[:4])
le = b[:4] == b'\xd4\xc3\xb2\xa1'
off, frames = 24, []
while off + 16 <= len(b):
    incl = struct.unpack('<I' if le else '>I', b[off+8:off+12])[0]
    frames.append(b[off+16:off+16+incl]); off += 16 + incl
if sys.argv[2] == 'count':
    assert len(frames) == int(sys.argv[3]), (len(frames), 'frames, wanted', sys.argv[3])
else:
    ip, gw = sys.argv[3], sys.argv[4]
    req = [f for f in frames if len(f) >= 35 and f[12:14] == b'\x08\x00' and f[23] == 1
           and f[34] == 8 and socket.inet_ntoa(f[26:30]) == ip and socket.inet_ntoa(f[30:34]) == gw]
    assert req, ('no ICMP echo request', ip, '->', gw, 'in', len(frames), 'frames')
PY
  check "net capture -c 2: stops after exactly two frames, to stdout" ok bash -c "
    $(declare -f e2e_capture); BIN='$BIN' L3C='$L3C' e2e_capture 2 '$L3/c2.pcap' >/dev/null
    python3 '$L3/pcap.py' '$L3/c2.pcap' count 2"
  check "net capture: the pcap carries the container's own ICMP echo requests" ok bash -c "
    $(declare -f e2e_capture); set -- \$(BIN='$BIN' L3C='$L3C' e2e_capture 40 '$L3/c40.pcap')
    python3 '$L3/pcap.py' '$L3/c40.pcap' icmp \"\$1\" \"\$2\""
else
  skip "net capture" "tcpdump is not installed"
fi
check "net capture: a container with no netns of its own is refused (1)" ok bash -c "
  out=\$('$BIN' net capture 'l3w-$PFX' -w - 2>&1); rc=\$?
  [ \$rc -eq 1 ] && grep -q 'no netns of its own' <<<\"\$out\""

# --- container attach: output only, and its refusal ---------------------------
# `attach` follows the output (it is `logs -f`), so the check cuts it after a
# few seconds and judges what it showed. `-i` is refused by design: no live
# stdin conduit to a detached container exists.
check "container attach -i is refused (1) and points at exec -it" ok bash -c "
  out=\$('$BIN' container attach -i '$L3C' 2>&1); rc=\$?
  [ \$rc -eq 1 ] && grep -q 'exec -it' <<<\"\$out\""
check "container attach: shows the output of a running container" ok bash -c "
  '$BIN' container run -d --name '$L3C-say' --net none '$IMG' sh -c 'echo hello-$PFX; exec sleep 600' >/dev/null || exit 1
  out=\$(timeout -s KILL 8 '$BIN' container attach '$L3C-say' 2>&1)
  grep -q 'hello-$PFX' <<<\"\$out\" || { echo \"attach did not show the output: \$out\"; exit 1; }"
e2e_rm "$L3C-say"
check "container attach: a container that does not exist is 4" 4 "$BIN" container attach "nao-existe-$PFX"

# --- system monitor --no-stream: one sample, and the count it is built on -----
check "system monitor --no-stream: one sample, counting the running containers on a network" ok bash -c "
  ids=\$('$BIN' container ls -q); [ -n \"\$ids\" ] || { echo 'no running container to count'; exit 1; }
  want=\$('$BIN' container inspect \$ids | python3 -c 'import json,sys; print(sum(1 for c in json.load(sys.stdin) if c.get(\"ip\") and c.get(\"status\")==\"Running\"))')
  out=\$(timeout -s KILL 30 '$BIN' system monitor --no-stream 2>&1) || exit 1
  grep -qE \"^delonix monitor — \$want containers, [0-9]+ active connections\" <<<\"\$out\" || { echo \"expected \$want containers in: \$(head -1 <<<\"\$out\")\"; exit 1; }"

# --- pod port-forward: a host port into the pod's own loopback ----------------
# busybox `nc -e` answers on connect; a `printf | nc -l` server only answers
# after the client half-closes, which a forwarded connection never does here.
cat >"$L3/pfpod.yaml" <<YAML
apiVersion: compute.delonix.io/v1alpha1
kind: Pod
metadata:
  name: l3pf$PFX
spec:
  containers:
    - name: srv
      image: $IMG
      command: ["nc", "-lk", "-p", "7070", "-e", "echo", "pong-$PFX"]
YAML
check "pod port-forward: a pod that does not exist is 4" 4 "$BIN" pod port-forward "nao-existe-$PFX" 7070
if check_then "pod port-forward: the pod applies" ok "$BIN" stack apply -f "$L3/pfpod.yaml"; then
  check "pod port-forward: a port spec that is not a port is refused (1)" 1 "$BIN" pod port-forward "l3pf$PFX" abc
  e2e_port L3FP
  check "pod port-forward: 127.0.0.1:<hostPort> reaches the pod's port" ok bash -c "
    timeout -s KILL 60 '$BIN' pod port-forward 'l3pf$PFX' '$L3FP:7070' >/dev/null 2>&1 & pf=\$!
    r=
    for _ in \$(seq 1 40); do
      r=\$(timeout 8 nc -w 3 127.0.0.1 $L3FP </dev/null 2>/dev/null)
      [ \"\$r\" = pong-$PFX ] && break
      sleep 0.3
    done
    kill \$pf 2>/dev/null; wait \$pf 2>/dev/null
    [ \"\$r\" = pong-$PFX ] || { echo \"got '\$r'\"; exit 1; }"
fi
"$BIN" delete pod "l3pf$PFX" -f >/dev/null 2>&1

# --- vm image: import, describe, convert, rm — a disk this engine did not build
# A 16 MiB empty qcow2 is enough: none of these four boots anything. `--appliance`
# is what decides whether `vm create` seeds cloud-init, and `describe` is where
# an operator reads it, so the flag is asserted back from `describe`.
if command -v qemu-img >/dev/null 2>&1; then
  L3VI="l3vi-$PFX"
  qemu-img create -f qcow2 "$L3/blank.qcow2" 16M >/dev/null 2>&1
  check "vm image import: registers the disk with the defaults it was given" ok bash -c "
    '$BIN' vm image import '$L3/blank.qcow2' -t '$L3VI:1' --appliance --default-vcpus 2 --default-memory 512M --no-compress >/dev/null 2>&1 || exit 1
    '$BIN' vm image ls 2>&1 | grep -q '$L3VI'"
  check "vm image describe: an appliance, with the vCPU/memory defaults" ok bash -c "
    out=\$('$BIN' vm image describe '$L3VI:1' 2>&1) || exit 1
    grep -qiE 'cloud-init: *no' <<<\"\$out\" || { echo 'the appliance flag is not reported'; printf '%s\n' \"\$out\"; exit 1; }
    grep -q '512M' <<<\"\$out\" || { echo 'the memory default is not reported'; exit 1; }"
  check "vm image convert --to vhd: qemu-img reads a vpc file (vhd is vpc to qemu-img)" ok bash -c "
    '$BIN' vm image convert '$L3VI:1' --to vhd -o '$L3/blank.vhd' >/dev/null || exit 1
    qemu-img info '$L3/blank.vhd' 2>/dev/null | grep -q 'file format: vpc'"
  check "vm image convert --compress to raw is refused (1): only qcow2 and vmdk compress" 1 \
    "$BIN" vm image convert "$L3VI:1" --to raw --compress -o "$L3/blank.raw"
  check "vm image rm: the image leaves the store, and describe answers 4" ok bash -c "
    '$BIN' vm image rm '$L3VI:1' >/dev/null || exit 1
    '$BIN' vm image describe '$L3VI:1' >/dev/null 2>&1; [ \$? -eq 4 ]"
else
  skip "vm image import/describe/convert/rm" "qemu-img is not installed"
fi
check "vm image init: a VMfile and a cloud-init file, filled in with the name" ok bash -c "
  d='$L3/vmfile'; rm -rf \"\$d\"
  '$BIN' vm image init 'l3img-$PFX' --dir \"\$d\" >/dev/null || exit 1
  grep -q '^FROM ' \"\$d/VMfile\" || { echo 'the VMfile has no FROM'; exit 1; }
  grep -q 'l3img-$PFX' \"\$d/VMfile\" || { echo 'the name is not in the VMfile'; exit 1; }
  [ -s \"\$d/cloud-init/user-data.yaml\" ] || { echo 'no cloud-init user-data'; exit 1; }"
check "vm image init: an existing VMfile is not overwritten without --force (1)" 1 \
  "$BIN" vm image init "l3img-$PFX" --dir "$L3/vmfile"

# --- vm apply: a disk that is neither a VM image nor a file -------------------
check "vm apply: a disk that is neither a VM image nor a file is 4, and no VM is left" ok bash -c "
  printf 'apiVersion: compute.delonix.io/v1alpha1\\nkind: VirtualMachine\\nmetadata:\\n  name: l3va-$PFX\\nspec:\\n  disk: /nao/existe-$PFX.qcow2\\n  memory: 64M\\n  vcpus: 1\\n' >'$L3/vm.yaml'
  '$BIN' vm apply -f '$L3/vm.yaml' >/dev/null 2>&1; rc=\$?
  [ \$rc -eq 4 ] || { echo \"expected 4, got \$rc\"; exit 1; }
  '$BIN' describe virtualmachines 'l3va-$PFX' >/dev/null 2>&1; [ \$? -eq 4 ] || { echo 'a VM record was left behind'; exit 1; }"

# --- vm reach / bridge / unbridge: read-only, and dry-run by default ----------
# `vm reach` lists the ports a VM could NOT reach: one published on the default
# 127.0.0.1 shows up as host-only. `vm bridge` and `unbridge` print a plan that
# needs root and change nothing without `--apply` — and `--apply` without root
# is refused before the first command runs. 192.0.2.0/24 (TEST-NET-1) stands in
# for a VM subnet.
e2e_port L3RP
if check_then "vm reach: the publishing container starts" ok \
     "$BIN" container run -d --name "$L3C-pub" --net "$L3N" -p "$L3RP:80" "$IMG" sleep 600; then
  check "vm reach: a port published on the default 127.0.0.1 is reported host-only" ok bash -c "
    '$BIN' vm reach 2>&1 | grep '$L3C-pub' | grep -q '127.0.0.1 (host only)'"
fi
e2e_rm "$L3C-pub"
check "vm bridge: a DRY-RUN plan that changes nothing on the host" ok bash -c "
  before=\$(ip -o link | wc -l)
  out=\$('$BIN' vm bridge '$L3N' --vm-subnet 192.0.2.0/24 2>&1) || { printf '%s\n' \"\$out\"; exit 1; }
  grep -q 'DRY-RUN' <<<\"\$out\" || { echo 'not announced as a dry-run'; exit 1; }
  grep -q 'FORWARD' <<<\"\$out\" || { echo 'the plan has no FORWARD rule'; exit 1; }
  [ \"\$(ip -o link | wc -l)\" = \"\$before\" ] || { echo 'a link appeared on the host'; exit 1; }"
check "vm bridge: a subnet that is not a CIDR is refused (1)" 1 "$BIN" vm bridge "$L3N" --vm-subnet default
check "vm unbridge: a DRY-RUN teardown plan" ok bash -c "
  '$BIN' vm unbridge '$L3N' --vm-subnet 192.0.2.0/24 2>&1 | grep -q 'DRY-RUN'"
if [ "$(id -u)" -ne 0 ]; then
  check "vm bridge --apply without root is refused (1) before any command runs" ok bash -c "
    before=\$(ip -o link | wc -l)
    out=\$('$BIN' vm bridge '$L3N' --vm-subnet 192.0.2.0/24 --apply 2>&1); rc=\$?
    [ \$rc -eq 1 ] || { echo \"expected 1, got \$rc\"; exit 1; }
    [ \"\$(ip -o link | wc -l)\" = \"\$before\" ]"
else
  skip "vm bridge --apply without root" "this run IS root"
fi

# This batch's own resources, by name — never a prune: the sweeps section after
# the cleanup judges what each prune takes, and a leftover here would be
# counted there.
e2e_rm "l3w-$PFX" "$L3C"
e2e_net_rm "$L3N"
rm -rf "$L3"
fi

section "container ps sem -a: só os que estão a correr, na CLI e na API Docker"
# `container ps` sem `-a` listava `Exited (0)` — um `stop`, ou uma saída com 0 —
# e escondia `Exited (3)` e `Dead`: a mesma situação listada ou escondida pelo
# código de saída. O `--help` («# the running ones») e o `cli-stability.md` (a
# semântica do `docker ps`) prometem só os que estão a correr. A API Docker
# ignorava o `all` e devolvia tudo, e perdia as portas publicadas: o parser dela
# esperava `hostPort:contPort` e o registo guarda `addr:hostPort:contPort/proto`
# desde 2026-10-07. Medido a 2026-10-11 nos três.
PSR="psrun-$PFX"; PSS="psstop-$PFX"; PSF="psfail-$PFX"; e2e_port PSPORT
# Creating the three is a CHECK, not a condition: a first version folded it into
# the `if` and skipped the whole section when one refused to start — which it
# did, because `-p` with `--net none` is refused (correctly). A gate that skips
# when its own setup fails proves nothing.
if [ "$E2E_HAVE_IMAGE" = 1 ]; then
  check "container ps: os três containers de prova arrancam" ok bash -c \
    "'$BIN' container run -d --name '$PSR' -p 127.0.0.1:$PSPORT:80 '$IMG' sleep 600 >/dev/null \
     && '$BIN' container run -d --name '$PSS' --net none '$IMG' sleep 600 >/dev/null \
     && '$BIN' container run -d --name '$PSF' --net none '$IMG' sh -c 'exit 3' >/dev/null"
  "$BIN" container stop -t 0 "$PSS" >/dev/null 2>&1
  for _ in $(seq 1 50); do
    "$BIN" container inspect "$PSF" 2>/dev/null | grep -qE '"status": *"Running"' || break; sleep 0.2
  done
  check "container ps lista o que está a correr" ok bash -c \
    "'$BIN' container ps | grep -qE ' $PSR\$'"
  check "container ps NÃO lista um container parado por stop" ok bash -c \
    "! '$BIN' container ps | grep -qE ' $PSS\$'"
  check "container ps NÃO lista um container que saiu com 3" ok bash -c \
    "! '$BIN' container ps | grep -qE ' $PSF\$'"
  check "container ps -a lista os três" ok bash -c \
    "[ \"\$('$BIN' container ps -a | grep -cE ' ($PSR|$PSS|$PSF)\$')\" = 3 ]"
  check "container ps -q e -o json seguem a mesma regra" ok bash -c \
    "'$BIN' container ps -o json | python3 -c \"import json,sys; n={r['name'] for r in json.load(sys.stdin)}; sys.exit(0 if '$PSR' in n and '$PSS' not in n and '$PSF' not in n else 1)\""
  PSSOCK="/tmp/dlx-srv-ps-$PFX.sock"
  PSPID="$(e2e_serve_up docker-api "$PSSOCK")"
  check "docker-api: GET /containers/json sem all só devolve os que estão a correr" ok bash -c \
    "n=\"\$(curl -s --unix-socket '$PSSOCK' --max-time 30 'http://localhost/containers/json' | python3 -c \"import json,sys; print(' '.join(c['Names'][0][1:] for c in json.load(sys.stdin)))\")\"; [[ \" \$n \" == *' $PSR '* && \" \$n \" != *' $PSS '* && \" \$n \" != *' $PSF '* ]]"
  check "docker-api: all=1 devolve também os parados" ok bash -c \
    "n=\"\$(curl -s --unix-socket '$PSSOCK' --max-time 30 'http://localhost/containers/json?all=1' | python3 -c \"import json,sys; print(' '.join(c['Names'][0][1:] for c in json.load(sys.stdin)))\")\"; [[ \" \$n \" == *' $PSS '* && \" \$n \" == *' $PSF '* ]]"
  check "docker-api: a porta publicada vem nos Ports, com o IP" ok bash -c \
    "curl -s --unix-socket '$PSSOCK' --max-time 30 'http://localhost/containers/json' | python3 -c \"import json,sys; p=[c['Ports'] for c in json.load(sys.stdin) if c['Names'][0]=='/$PSR']; sys.exit(0 if p and {'PrivatePort':80,'PublicPort':$PSPORT,'Type':'tcp','IP':'127.0.0.1'} in p[0] else 1)\""
  [ -n "$PSPID" ] && kill "$PSPID" 2>/dev/null
  for _ in $(seq 1 40); do kill -0 "$PSPID" 2>/dev/null || break; sleep 0.2; done
  rm -f "$PSSOCK"
else
  skip "container ps sem -a" "sem imagem de teste"
fi
e2e_rm "$PSR" "$PSS" "$PSF"

section "limpeza"
########################################
e2e_rm "$C"
check "volume rm" ok "$BIN" volume rm "$VOL"
check "network rm" ok "$BIN" network rm "$NET"

section "as varreduras: o que cada prune leva, e o que NÃO leva"
########################################
# Oito folhas de poda, nenhuma delas executada até aqui — e vêm DEPOIS da
# limpeza de propósito. Um `image prune` a meio da bateria tirava a imagem
# base debaixo das secções seguintes, e um `container prune` levava o
# container que a secção ao lado ainda ia inspeccionar. Correm no root
# ISOLADO desta corrida (ver o cabeçalho), nunca no do utilizador.
#
# O que cada check assere é a FRONTEIRA de cada varredura, não o rc: um prune
# que leva mais do que promete é a classe de bug mais cara que este grupo pode
# ter, e um que leve menos só custa espaço.

# --- a imagem COM tag sobrevive a um prune sem `--all` -----------------------
# O `--help` promete: «by default only the DANGLING ones (no tag); `--all` also
# drops tagged images that no container uses». Se isto regredir, cada corrida
# da bateria passa a re-descarregar a imagem base — e o custo aparece como
# lentidão de rede, não como um prune a mais.
check "image prune: a imagem COM tag fica, e os blobs pendentes saem" ok bash -c "
  '$BIN' image prune -f >/dev/null || exit 1
  '$BIN' image ls 2>&1 | grep -qF '$IMG' || { echo 'o prune sem --all levou a imagem com tag'; '$BIN' image ls; exit 1; }"

check "container prune: sem containers parados, leva zero e di-lo" ok bash -c "
  '$BIN' container prune -f 2>&1 | grep -qE 'removed: [0-9]+ container'"

check "volume prune: um volume ainda REFERENCIADO não é removido" ok bash -c "
  '$BIN' volume create vp-$PFX >/dev/null 2>&1
  '$BIN' container run -d --name vpc-$PFX --net none -v vp-$PFX:/d '$IMG' sleep 300 >/dev/null 2>&1 || { echo 'não consegui criar o container de referência'; exit 1; }
  '$BIN' volume prune -f >/dev/null || exit 1
  '$BIN' volume ls 2>&1 | grep -q 'vp-$PFX' || { echo 'o prune levou um volume que um container monta'; exit 1; }
  '$BIN' container rm -f vpc-$PFX >/dev/null 2>&1
  '$BIN' volume prune -f >/dev/null
  '$BIN' volume ls 2>&1 | grep -q 'vp-$PFX' && { echo 'já sem container, o volume devia ter saído'; exit 1; }
  true"

check "network ipam prune: um lease é CANDIDATO na 1ª passagem, nunca reclamado" ok bash -c "
  out=\$('$BIN' network ipam prune 2>&1) || exit 1
  grep -qE 'reclaimed [0-9]+ orphaned lease' <<<\"\$out\""

check "vm prune: sem VMs, leva zero órfãos e di-lo" ok bash -c "
  '$BIN' vm prune -f 2>&1 | grep -qE 'removed: [0-9]+ orphan'"

check "cluster prune: sem clusters, leva zero restos" ok bash -c "
  '$BIN' cluster prune -f 2>&1 | grep -qE 'removed: [0-9]+ cluster'"

# --- `stack prune` é a metade de PODA do apply, e exige o manifesto ----------
# Sem manifesto não há autorização: o manifesto É a autorização (ver a secção
# do IaC no AGENTS.md). Um ficheiro que não existe tem de recusar, não de
# varrer tudo o que a stack possui.
check "stack prune: sem o manifesto, RECUSA — não varre a stack às cegas" ok bash -c "
  out=\$('$BIN' stack prune -f '$OUT/nao-existe-$PFX.yaml' 2>&1); rc=\$?
  [ \$rc -ne 0 ] || { echo 'aceitou podar sem o manifesto que o autoriza'; exit 1; }
  grep -q 'could not read' <<<\"\$out\" || { echo 'recusou sem dizer o que não leu'; exit 1; }"

# --- e a varredura global, em último lugar -----------------------------------
check "system prune: varre o nó e a imagem com tag CONTINUA lá" ok bash -c "
  '$BIN' system prune -f >/dev/null || exit 1
  '$BIN' image ls 2>&1 | grep -qF '$IMG' || { echo 'o system prune levou a imagem com tag'; exit 1; }"

section "kind: Service type VirtualIP — o VIP L4 com prontidão (ADR-0066)"
########################################
# The VIP is judged by its EFFECT: which backend answered each connection, the
# DNAT counter in the holder, and the spoof drop counter — never the rc of the
# apply. A VIP that served one backend only would answer too, so every spread
# check counts per backend.
#
# Only in an isolated run: the pool is written to the root's `ingress/vip-pool`
# and the checks read the counters inside THAT root's holder.
if [[ "$E2E_ISOLATED" != 1 ]]; then
  skip "VirtualIP" "estado partilhado — o conjunto de VIP e o holder seriam os reais"
else
  VN="vn-$PFX"; VNS="vipa$$"; VNB="vipb$$"; VW="$OUT/vip-www"; VIMG="busybox:1.36"
  VPOOL="${E2E_VIP_POOL:-10.97.0.0/24}"
  vpin() { cat "$DELONIX_ROOT/ingress/holder.pid" 2>/dev/null; }
  vnft() { nsenter -t "$(vpin)" -U -n --preserve-credentials nft "$@"; }
  vcount() { # <client> <vip> <port> <n>: one line per connection, then counted
    for _ in $(seq 1 "$4"); do "$BIN" container exec "$1" wget -qO- -T 2 "http://$2:$3/" 2>&1; done | sort | uniq -c
  }
  vstate() { "$BIN" describe services "$1" 2>/dev/null | grep -E "^  $2:" ; }
  vwait() { # <svc> <backend> <regex>: up to 20 s for the backend's state
    for _ in $(seq 1 100); do vstate "$1" "$2" | grep -qE "$3" && return 0; sleep 0.2; done; return 1
  }
  vsvc() { # <name> <ns> <app> [vip] [host port to publish]
    printf 'apiVersion: networking.delonix.io/v1alpha1\nkind: Service\nmetadata: {name: %s, namespace: %s}\nspec:\n  selector: {matchLabels: {app: %s}}\n  port: 8080\n  type: VirtualIP\n%s%s' \
      "$1" "$2" "$3" "${4:+  vip: $4
}" "${5:+  publish: [$5]
}" > "$OUT/vsvc-$1.yaml"
    echo "$OUT/vsvc-$1.yaml"
  }
  mkdir -p "$VW/v1" "$VW/v2" "$VW/vh"
  echo v1 > "$VW/v1/index.html"; echo v2 > "$VW/v2/index.html"; echo vh > "$VW/vh/index.html"
  "$BIN" network vip-pool clear >/dev/null 2>&1
  check "VirtualIP: sem conjunto de VIP é recusado com a classe 69" 69 \
    env -u DELONIX_SERVICE_VIP_CIDR "$BIN" apply -f "$(vsvc vweb "$VNS" vweb)"
  check "VirtualIP: um conjunto público é recusado (DX-1360)" 1 "$BIN" network vip-pool set 8.8.8.0/24
  # The image is the host's half (a registry this host reaches, or the image
  # already in the store — the same rule as the `image` section's guard); the
  # network is the engine's, and judged.
  if ! grep -qF "$VIMG " <<<"$("$BIN" image ls 2>&1)" && ! "$BIN" image pull "$VIMG" >/dev/null 2>&1; then
    skip "VirtualIP: o resto da secção" "sem rede (ou registo inalcançável) e $VIMG não está no store"
  elif check_then "VirtualIP: a rede dos backends cria-se" ok "$BIN" network create "$VN" --subnet 10.232.0.0/16; then
    check "VirtualIP: um conjunto sobreposto a uma rede declarada é recusado e nomeia-a" ok bash -c \
      "out=\$('$BIN' network vip-pool set 10.232.4.0/24 2>&1); rc=\$?; [ \$rc -eq 5 ] && grep -q 'engine network $VN' <<<\"\$out\""
    # Overlap with the HOST's routes is read here, before the engine is asked;
    # a refusal on a pool no host route touches is the engine's, and a FAIL.
    _vclash=$(ip -4 route 2>/dev/null | python3 -c '
import ipaddress, sys
pool = ipaddress.ip_network(sys.argv[1])
for line in sys.stdin:
    try:
        net = ipaddress.ip_network(line.split()[0], strict=False)
    except (ValueError, IndexError):
        continue
    if net.prefixlen and net.overlaps(pool):
        print(net, end=" ")' "$VPOOL")
    if [[ -n "$_vclash" ]]; then
      skip "VirtualIP: o resto da secção" "o conjunto $VPOOL sobrepõe-se a rotas deste host: $_vclash(E2E_VIP_POOL muda-o)"
    elif ! check_then "VirtualIP: vip-pool set $VPOOL" ok "$BIN" network vip-pool set "$VPOOL"; then
      :  # já é um FAIL, com a recusa do motor
    else
      check "VirtualIP: vip-pool show nomeia o conjunto e o ficheiro do nó de onde vem" ok bash -c \
        "env -u DELONIX_SERVICE_VIP_CIDR '$BIN' network vip-pool show -o json | python3 -c 'import json,sys; d=json.load(sys.stdin); sys.exit(0 if d[\"pools\"]==[\"$VPOOL\"] and d[\"source\"].endswith(\"ingress/vip-pool\") else 1)'"
      check "VirtualIP: vip-pool show diz quando a variável do processo se sobrepõe ao ficheiro" ok bash -c \
        "out=\$(DELONIX_SERVICE_VIP_CIDR=10.233.250.0/24 '$BIN' network vip-pool show); grep -q '^10.233.250.0/24\$' <<<\"\$out\" && grep -q 'source: DELONIX_SERVICE_VIP_CIDR' <<<\"\$out\""
      # Judged, not silent: a backend that did not start used to surface only
      # as a spread check FAILing far from its cause.
      for b in v1 v2; do
        check "VirtualIP: o backend $b arranca" ok "$BIN" container run -d --name "$b-$PFX" --net "$VN" --namespace "$VNS" --label app=vweb \
          -v "$VW/$b:/www" "$VIMG" httpd -f -p 8080 -h /www
      done
      check "VirtualIP: o cliente da mesma namespace arranca" ok \
        "$BIN" container run -d --name "vc-$PFX" --net "$VN" --namespace "$VNS" "$VIMG" sleep 900
      check "VirtualIP: o cliente de outra namespace arranca" ok \
        "$BIN" container run -d --name "vx-$PFX" --net "$VN" --namespace "$VNB" "$VIMG" sleep 900
      check "VirtualIP: o contentor que forja origens arranca" ok \
        "$BIN" container run -d --name "vs-$PFX" --cap-add NET_ADMIN --net "$VN" --namespace "$VNB" "$VIMG" sleep 900
      # D5 before any VIP exists: once one does, the holder puts the sysctl
      # back to 1 on its next round.
      nsenter -t "$(vpin)" -U -n --preserve-credentials sh -c 'echo 0 > /proc/sys/net/bridge/bridge-nf-call-iptables' 2>/dev/null
      check "VirtualIP: sem bridge-nf-call-iptables no holder é recusado com a classe 69 (DX-6361)" ok bash -c \
        "out=\$('$BIN' apply -f '$(vsvc vbr "$VNS" vbr)' 2>&1); rc=\$?; echo \"\$out\"; [ \$rc -eq 69 ] && grep -q DX-6361 <<<\"\$out\""
      nsenter -t "$(vpin)" -U -n --preserve-credentials sh -c 'echo 1 > /proc/sys/net/bridge/bridge-nf-call-iptables' 2>/dev/null
      VHP=$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1])')
      check "VirtualIP: o apply atribui um VIP do conjunto" ok "$BIN" apply -f "$(vsvc vweb "$VNS" vweb "" "$VHP")"
      VIP=$("$BIN" get services -o json 2>/dev/null | python3 -c 'import json,sys;print(next(r["vip"] for r in json.load(sys.stdin) if r["name"]=="vweb"))' 2>/dev/null)
      vwait vweb "v1-$PFX" 'ready \(tcp\)'; vwait vweb "v2-$PFX" 'ready \(tcp\)'
      check "VirtualIP: o nome do serviço resolve para o VIP" ok bash -c \
        "'$BIN' container exec vc-$PFX nslookup -type=a vweb.$VNS.svc.delonix.internal 2>&1 | grep -q 'Address: $VIP\$'"
      SPREAD=$(vcount "vc-$PFX" "$VIP" 8080 60)
      check "VirtualIP: o VIP espalha as ligações por todos os backends prontos (cada um >= 20%)" ok bash -c \
        "c1=\$(awk '\$2==\"v1\"{print \$1}' <<<'$SPREAD'); c2=\$(awk '\$2==\"v2\"{print \$1}' <<<'$SPREAD'); [ \"\${c1:-0}\" -ge 12 ] && [ \"\${c2:-0}\" -ge 12 ] && [ \$((c1+c2)) -eq 60 ]"
      HSPREAD=$(for _ in $(seq 1 40); do curl -s -m 2 "http://127.0.0.1:$VHP/"; done | sort | uniq -c)
      check "VirtualIP: a porta publicada leva ao mesmo conjunto (D6)" ok bash -c \
        "c1=\$(awk '\$2==\"v1\"{print \$1}' <<<'$HSPREAD'); c2=\$(awk '\$2==\"v2\"{print \$1}' <<<'$HSPREAD'); [ \"\${c1:-0}\" -ge 8 ] && [ \"\${c2:-0}\" -ge 8 ] && [ \$((c1+c2)) -eq 40 ]"
      check "VirtualIP: o cliente na rede dos backends chega com o seu IP (hairpin, conntrack)" ok bash -c \
        "CIP=\$('$BIN' container inspect vc-$PFX | python3 -c 'import json,sys;d=json.load(sys.stdin);d=d[0] if isinstance(d,list) else d;print(d[\"ip\"])'); nsenter -t $(vpin) -U -n --preserve-credentials conntrack -L -d $VIP 2>/dev/null | grep -q \"src=\$CIP dst=$VIP .* dst=\$CIP \""
      check "VirtualIP: um cliente de outra namespace não passa do VIP" fail \
        "$BIN" container exec "vx-$PFX" wget -qO- -T 2 "http://$VIP:8080/"
      # Anti-spoof: forging a same-namespace member must not even reach the DNAT.
      CIP=$("$BIN" container inspect "vc-$PFX" | python3 -c 'import json,sys;d=json.load(sys.stdin);d=d[0] if isinstance(d,list) else d;print(d["ip"])')
      vdnat() { vnft list chain ip dlxing svcpre | awk -v v="$VIP" '$0~v{for(i=1;i<=NF;i++) if($i=="packets") print $(i+1)}'; }
      d0=$(vdnat)
      "$BIN" container exec "vs-$PFX" ip addr add "$CIP/16" dev eth0 >/dev/null 2>&1
      "$BIN" container exec "vs-$PFX" nc -w 2 -s "$CIP" "$VIP" 8080 </dev/null >/dev/null 2>&1
      d1=$(vdnat)
      # The counter must be READ (a failed nsenter would make 0 - 0 look like a
      # pass), and a legitimate connection right after must move it.
      "$BIN" container exec "vs-$PFX" ip addr del "$CIP/16" dev eth0 >/dev/null 2>&1
      "$BIN" container exec "vc-$PFX" wget -qO- -T 2 "http://$VIP:8080/" >/dev/null 2>&1
      d2=$(vdnat)
      check "VirtualIP: uma origem forjada não chega ao DNAT (anti-spoof)" ok bash -c \
        "[ -n '$d0' ] && [ -n '$d2' ] && [ $(( ${d1:-0} - ${d0:-0} )) -eq 0 ] && [ $(( ${d2:-0} - ${d1:-0} )) -ge 1 ]"
      # A backend whose port stops accepting while its process stays up.
      "$BIN" container rm -f "v2-$PFX" >/dev/null 2>&1
      "$BIN" container run -d --name "v2-$PFX" --net "$VN" --namespace "$VNS" --label app=vweb \
        -v "$VW/v2:/www" "$VIMG" sleep 900 >/dev/null 2>&1
      "$BIN" container exec "v2-$PFX" httpd -p 8080 -h /www >/dev/null 2>&1
      vwait vweb "v2-$PFX" 'ready \(tcp\)'
      "$BIN" container exec "v2-$PFX" killall httpd >/dev/null 2>&1
      check "VirtualIP: um backend cuja porta deixa de aceitar sai da rotação" ok vwait vweb "v2-$PFX" 'not ready$'
      check "VirtualIP: e não recebe ligações novas (0 de 30)" ok bash -c \
        "! ( for i in \$(seq 1 30); do '$BIN' container exec vc-$PFX wget -qO- -T 2 http://$VIP:8080/ 2>&1; done | grep -qv '^v1\$' )"
      # A backend gated by its health check.
      "$BIN" container run -d --name "vh-$PFX" --net "$VN" --namespace "$VNS" --label app=vweb -v "$VW/vh:/www" \
        --health-cmd 'test -f /www/ok' --health-interval 1 --health-retries 1 "$VIMG" httpd -f -p 8080 -h /www >/dev/null 2>&1
      sleep 3
      check "VirtualIP: um backend cujo health check não passa não recebe tráfego" ok bash -c \
        "! ( for i in \$(seq 1 30); do '$BIN' container exec vc-$PFX wget -qO- -T 2 http://$VIP:8080/ 2>&1; done | grep -q '^vh\$' )"
      touch "$VW/vh/ok"
      check "VirtualIP: com o health check a passar entra em rotação" ok vwait vweb "vh-$PFX" '[0-9] healthy$'
      rm -f "$VW/vh/ok"
      check "VirtualIP: e sai quando volta a falhar" ok vwait vweb "vh-$PFX" 'health: unhealthy'
      # A backend IN ROTATION is removed: the map must drop it before `rm`
      # returns (the detach's `nsleave`), not one probe interval later — a
      # removed backend that was already out would prove nothing.
      "$BIN" container exec "v2-$PFX" httpd -p 8080 -h /www >/dev/null 2>&1
      check "VirtualIP: um backend que volta a aceitar regressa à rotação" ok vwait vweb "v2-$PFX" 'ready \(tcp\)'
      "$BIN" container rm -f "v2-$PFX" "vh-$PFX" >/dev/null 2>&1
      check "VirtualIP: um backend removido não recebe ligações novas" ok bash -c \
        "! ( for i in \$(seq 1 30); do '$BIN' container exec vc-$PFX wget -qO- -T 2 http://$VIP:8080/ 2>&1; done | grep -qv '^v1\$' )"
      check "VirtualIP: um porto do VIP que não é servido é recusado" ok bash -c \
        "'$BIN' container exec vc-$PFX wget -qO- -T 3 http://$VIP:9999/ 2>&1 | grep -q 'refused'"
      "$BIN" container stop "v1-$PFX" >/dev/null 2>&1
      check "VirtualIP: sem backend pronto o VIP recusa de imediato" ok bash -c \
        "t0=\$(date +%s%N); out=\$('$BIN' container exec vc-$PFX wget -qO- -T 5 http://$VIP:8080/ 2>&1); t1=\$(date +%s%N); echo \"\$out\"; grep -q refused <<<\"\$out\" && [ \$(( (t1-t0)/1000000 )) -lt 2000 ]"
      check "VirtualIP: sem backend pronto a porta publicada também recusa" ok bash -c \
        "t0=\$(date +%s%N); curl -s -m 5 http://127.0.0.1:$VHP/; rc=\$?; t1=\$(date +%s%N); echo rc=\$rc; case \$rc in 7|52|56) ;; *) exit 1;; esac; [ \$(( (t1-t0)/1000000 )) -lt 2000 ]"
      check "VirtualIP: um VIP pedido e já tomado é um conflito (5)" 5 \
        "$BIN" apply -f "$(vsvc vdup "$VNS" vdup "$VIP")"
      check "VirtualIP: um VIP pedido fora do conjunto é recusado (DX-1361)" 1 \
        "$BIN" apply -f "$(vsvc vout "$VNS" vout 10.98.0.9)"
      check "VirtualIP: o delete liberta o lease do VIP" ok bash -c \
        "'$BIN' delete services $VNS/vweb >/dev/null 2>&1; ! '$BIN' network ipam ls 2>/dev/null | grep -q 'svc:$VNS/vweb'"
      check "VirtualIP: o delete tira a porta publicada do host" 7 curl -s -m 3 "http://127.0.0.1:$VHP/"
    fi
  fi
  for c in v1 v2 vh vc vx vs; do "$BIN" container rm -f "$c-$PFX" >/dev/null 2>&1; done
  "$BIN" delete services $VNS/vweb $VNS/vbr >/dev/null 2>&1
  "$BIN" network rm "$VN" >/dev/null 2>&1
  check "VirtualIP: vip-pool clear tira o conjunto do nó, e o show diz que não há nenhum" ok bash -c \
    "'$BIN' network vip-pool clear >/dev/null && env -u DELONIX_SERVICE_VIP_CIDR '$BIN' network vip-pool show -o json | python3 -c 'import json,sys; d=json.load(sys.stdin); sys.exit(0 if d==dict(pools=[], source=\"none\") else 1)'"
fi

########################################
# kind: GitOpsSource (ADR-0021, Sprint 24): uma passagem converge a stack do
# commit, um campo quente converge com o MESMO PID, e um commit que o guarda, o
# grafo ou a política de segurança recusam deixa a stack em execução INTACTA e
# fica registado com o commit que nomeia. O repositório é um bare local
# (`file://`), nada vai à rede. O temporizador é escrito num HOME falso com um
# `systemctl` falso à frente no PATH: esta bateria nunca toca no gestor de
# utilizador real (o `systemctl` real nunca é chamado).
section "gitops: kind GitOpsSource — uma passagem converge, um commit recusado não toca em nada (ADR-0021)"
if ! command -v git >/dev/null 2>&1; then
  skip "gitops: kind GitOpsSource" "git não está instalado neste host"
elif [ -z "${IMG:-}" ] || ! "$BIN" image ls 2>/dev/null | grep -qF "${IMG%%:*}"; then
  skip "gitops: kind GitOpsSource" "a imagem $IMG não está no store desta corrida"
else
  GO="$OUT/gitops-$PFX"
  rm -rf "$GO"; mkdir -p "$GO/home/.config" "$GO/bin" "$GO/work/env/prod"
  printf '#!/bin/sh\necho "systemctl $*" >> "%s/systemctl.log"\nexit 0\n' "$GO" >"$GO/bin/systemctl"
  printf '#!/bin/sh\necho Linger=yes\n' >"$GO/bin/loginctl"
  chmod +x "$GO/bin/systemctl" "$GO/bin/loginctl"
  GS="${PFX}-gs"
  # Duas stacks, nunca a mesma (G1): a do bootstrap só declara a fonte; as
  # passagens aplicam o repositório em `gitops.<ns>.<nome>`.
  GSB="${PFX}-gsboot"
  GSS="gitops.default.$GS"
  # O mesmo ambiente que o unit do temporizador dá à passagem, mais o HOME falso.
  gox() { env HOME="$GO/home" XDG_CONFIG_HOME="$GO/home/.config" PATH="$GO/bin:$PATH" "$BIN" "$@"; }
  gogit() { git -C "$GO/work" -c user.name=e2e -c user.email=e2e@example.invalid -c commit.gpgsign=false "$@"; }
  gopush() { gogit add -A >/dev/null && gogit commit -q -m "$1" && gogit push -q --force "$GO/repo.git" main && gogit rev-parse HEAD; }
  gostate() { "$BIN" container inspect "${PFX}-gweb" 2>/dev/null \
    | python3 -c 'import json,sys;d=json.load(sys.stdin)[0];print(d["pid"],d["memory_max"])'; }
  goapplied() { gox gitops status "$GS" -o json \
    | python3 -c 'import json,sys;r=json.load(sys.stdin)[0];print(r["result"],r["commit"],r["applied"])'; }
  export -f gox gogit gopush gostate goapplied
  # The checks that exercise a gitops leaf call the binary DIRECTLY (not through
  # `gox`): cli_exec_ratchet.py counts a leaf only when the engine binary is a
  # word of the asserted command, and a shell function hides it.
  GOX="env HOME='$GO/home' XDG_CONFIG_HOME='$GO/home/.config' PATH='$GO/bin:$PATH' '$BIN'"
  export GO GS GSB GSS BIN PFX IMG

  git init -q --bare "$GO/repo.git"; gogit init -q -b main
  goapp() { # $1 = memory, $2 = extra spec lines (já indentados)
    cat >"$GO/work/env/prod/app.yaml" <<YAML
apiVersion: networking.delonix.io/v1alpha1
kind: Network
metadata: {name: ${PFX}-gnet}
spec: {}
---
apiVersion: compute.delonix.io/v1alpha1
kind: Container
metadata: {name: ${PFX}-gweb}
spec:
  image: $IMG
  network: ${PFX}-gnet
  command: ["sleep", "infinity"]
  memory: $1
$2
YAML
  }
  goapp 64M ""
  GOC1=$(gopush v1)
  cat >"$GO/source.yaml" <<YAML
apiVersion: gitops.delonix.io/v1alpha1
kind: GitOpsSource
metadata: {name: $GS}
spec:
  source:
    url: file://$GO/repo.git
    path: env/prod
    interval: 30s
  sync:
    automated: true
YAML

  check "gitops: stack apply regista a fonte e escreve o temporizador (systemctl falso)" ok bash -c "
    gox stack apply -f '$GO/source.yaml' --name '$GSB' >/dev/null || exit 1
    test -f '$GO/home/.config/systemd/user/delonix-gitops-default.$GS.timer' || { echo 'sem .timer'; exit 1; }
    grep -q 'ExecStart=.* gitops reconcile $GS --namespace default' '$GO/home/.config/systemd/user/delonix-gitops-default.$GS.service' || { echo 'ExecStart errado'; exit 1; }
    grep -q 'enable --now delonix-gitops-default.$GS.timer' '$GO/systemctl.log' || { echo 'o timer não foi activado'; exit 1; }"
  check "gitops: um manifesto inalterado planeia ZERO diferenças" 0 \
    gox stack plan -f "$GO/source.yaml" --name "$GSB" --detailed-exitcode
  check "gitops: um plan à mão da stack das passagens é recusado (DX-1601), a nomear --name e spec.stack" ok bash -c "
    out=\$(gox stack plan -f '$GO/source.yaml' --name '$GSS' 2>&1) && { echo 'planeou'; exit 1; }
    grep -q 'DX-1601' <<<\"\$out\" || { echo \"sem DX-1601: \$out\"; exit 1; }
    grep -q -- '--name' <<<\"\$out\" && grep -q 'spec.stack' <<<\"\$out\" || { echo \"não nomeia as saídas: \$out\"; exit 1; }"
  check "gitops: um nome de fonte que não é um rótulo DNS é recusado (DX-1601)" ok bash -c "
    sed 's/name: $GS/name: Bad_Name/' '$GO/source.yaml' >'$GO/bad.yaml'
    out=\$(gox stack apply -f '$GO/bad.yaml' --name x 2>&1) && { echo 'aceitou'; exit 1; }
    grep -q 'DX-1601' <<<\"\$out\" || { echo 'recusou sem o código DX-1601'; exit 1; }"
  check "gitops: manifest validate recusa a fonte que o apply recusa (url ext::, DX-1601)" ok bash -c "
    sed 's|url: file://.*|url: ext::sh -c id|' '$GO/source.yaml' >'$GO/badurl.yaml'
    out=\$('$BIN' manifest validate -f '$GO/badurl.yaml' 2>&1) && { echo 'validate disse OK'; exit 1; }
    grep -q 'DX-1601' <<<\"\$out\" || { echo \"sem DX-1601: \$out\"; exit 1; }"

  check "gitops: a primeira passagem converge a stack do commit" ok bash -c "
    $GOX gitops reconcile '$GS' >/dev/null || exit 1
    $GOX gitops status '$GS' | grep -q '$GS' || { echo 'o status não nomeia a fonte'; exit 1; }
    s=\$(goapplied); [ \"\$s\" = 'synced $GOC1 $GOC1' ] || { echo \"status: \$s\"; exit 1; }
    gostate | grep -q ' 64M\$' || { echo \"container: \$(gostate)\"; exit 1; }"
  GOPID=$(gostate | cut -d' ' -f1)

  goapp 96M ""; GOC2=$(gopush 'v2: memory')
  check "gitops: um campo quente (memory) converge na passagem seguinte com o MESMO PID" ok bash -c "
    $GOX gitops reconcile '$GS' >/dev/null || exit 1
    [ \"\$(gostate)\" = '$GOPID 96M' ] || { echo \"esperava '$GOPID 96M', está \$(gostate)\"; exit 1; }
    [ \"\$(goapplied)\" = 'synced $GOC2 $GOC2' ] || { echo \"status: \$(goapplied)\"; exit 1; }"
  check "gitops: uma passagem sem mudanças não escreve revisão nova" ok bash -c "
    a=\$(ls '$DELONIX_ROOT/stacks/$GSS/revisions' 2>/dev/null | wc -l)
    [ \"\$a\" -gt 0 ] || { echo 'as passagens anteriores não deixaram revisão nenhuma'; exit 1; }
    $GOX gitops reconcile '$GS' | grep -q 'in sync' || exit 1
    b=\$(ls '$DELONIX_ROOT/stacks/$GSS/revisions' 2>/dev/null | wc -l)
    [ \"\$a\" = \"\$b\" ] || { echo \"revisões \$a -> \$b\"; exit 1; }"

  # G1: a stack do bootstrap e a das passagens são duas. O plan do bootstrap
  # fica em 0 e um --prune dele nunca toca no que a passagem aplicou; um
  # --prune da stack das passagens a partir do bootstrap é recusado antes de
  # qualquer efeito.
  check "gitops: o plan do bootstrap fica em 0 depois das passagens" 0 \
    gox stack plan -f "$GO/source.yaml" --name "$GSB" --detailed-exitcode
  check "gitops: um --prune do bootstrap não toca no que a passagem aplicou" ok bash -c "
    gox stack apply --prune -f '$GO/source.yaml' --name '$GSB' >/dev/null || exit 1
    [ \"\$(gostate)\" = '$GOPID 96M' ] || { echo \"a stack da passagem mexeu: \$(gostate)\"; exit 1; }
    '$BIN' network inspect '${PFX}-gnet' >/dev/null 2>&1 || { echo 'a rede da passagem foi apagada'; exit 1; }"
  check "gitops: um --prune da stack das passagens com o manifesto do bootstrap é recusado e não apaga nada" ok bash -c "
    out=\$(gox stack apply --prune -f '$GO/source.yaml' --name '$GSS' 2>&1) && { echo 'aplicou'; exit 1; }
    grep -q 'DX-1601' <<<\"\$out\" || { echo \"sem DX-1601: \$out\"; exit 1; }
    [ \"\$(gostate)\" = '$GOPID 96M' ] || { echo \"a stack mexeu: \$(gostate)\"; exit 1; }"

  # O repositório declara-se a si próprio: a passagem toma o registo da fonte
  # à stack do bootstrap, e daí em diante é a stack das passagens que a possui.
  cp "$GO/source.yaml" "$GO/work/env/prod/source.yaml"; GOCS=$(gopush 'self: the repository declares its source')
  check "gitops: uma fonte que se declara no repositório é tomada à stack do bootstrap" ok bash -c "
    out=\$($GOX gitops reconcile '$GS' 2>&1) || { echo \"recusou: \$out\"; exit 1; }
    grep -q \"taken over from stack '$GSB'\" <<<\"\$out\" || { echo \"sem a tomada: \$out\"; exit 1; }
    [ \"\$(goapplied)\" = 'synced $GOCS $GOCS' ] || { echo \"status: \$(goapplied)\"; exit 1; }
    $GOX gitops reconcile '$GS' | grep -q 'in sync' || { echo 'a segunda passagem não está em sincronia'; exit 1; }
    [ \"\$(gostate)\" = '$GOPID 96M' ] || { echo \"a stack mexeu: \$(gostate)\"; exit 1; }
    gox stack apply --prune -f '$GO/source.yaml' --name '$GSB' >/dev/null 2>&1 && { echo 'o bootstrap reaplicou uma fonte que já não é sua'; exit 1; }
    [ \"\$(gostate)\" = '$GOPID 96M' ] || { echo \"o --prune do bootstrap mexeu: \$(gostate)\"; exit 1; }"

  goapp 128M "  privileged: true"; GOC3=$(gopush 'broken: privileged')
  check "gitops: um commit que pede o host é recusado (DX-1602) e a stack fica intacta" ok bash -c "
    out=\$($GOX gitops reconcile '$GS' 2>&1); rc=\$?
    [ \$rc -eq 1 ] || { echo \"rc=\$rc\"; exit 1; }
    grep -q 'DX-1602' <<<\"\$out\" || { echo 'sem DX-1602'; exit 1; }
    [ \"\$(gostate)\" = '$GOPID 96M' ] || { echo \"a stack mexeu: \$(gostate)\"; exit 1; }
    [ \"\$(goapplied)\" = 'rejected $GOC3 $GOCS' ] || { echo \"status: \$(goapplied)\"; exit 1; }"

  goapp 128M ""; sed -i "s/network: ${PFX}-gnet/network: ${PFX}-nosuch/" "$GO/work/env/prod/app.yaml"
  GOC4=$(gopush 'broken: graph')
  check "gitops: um commit cujo grafo falha é recusado e a stack fica intacta" ok bash -c "
    $GOX gitops reconcile '$GS' >/dev/null 2>&1 && { echo 'aceitou'; exit 1; }
    [ \"\$(gostate)\" = '$GOPID 96M' ] || { echo \"a stack mexeu: \$(gostate)\"; exit 1; }
    [ \"\$(goapplied)\" = 'rejected $GOC4 $GOCS' ] || { echo \"status: \$(goapplied)\"; exit 1; }"

  goapp 96M ""; sed -i "s|image: $IMG|image: ${IMG%%:*}:gitops-nao-existe|" "$GO/work/env/prod/app.yaml"
  GOC5=$(gopush 'blocked: replace')
  check "gitops: um commit que planeia um Replace é bloqueado (DX-1604) e a stack fica intacta" ok bash -c "
    out=\$($GOX gitops reconcile '$GS' 2>&1); rc=\$?
    [ \$rc -eq 1 ] || { echo \"rc=\$rc\"; exit 1; }
    grep -q 'DX-1604' <<<\"\$out\" || { echo 'sem DX-1604'; exit 1; }
    [ \"\$(gostate)\" = '$GOPID 96M' ] || { echo \"a stack mexeu: \$(gostate)\"; exit 1; }
    [ \"\$(goapplied)\" = 'blocked $GOC5 $GOCS' ] || { echo \"status: \$(goapplied)\"; exit 1; }"

  goapp 96M ""; GOC6=$(gopush 'v6: back to a good commit')
  check "gitops: suspensa, uma passagem não obtém nada; retomada, a seguinte está em sincronia" ok bash -c "
    $GOX gitops suspend '$GS' >/dev/null || exit 1
    $GOX gitops reconcile '$GS' | grep -q 'suspended' || exit 1
    $GOX gitops resume '$GS' >/dev/null || { echo 'resume falhou'; exit 1; }
    out=\$($GOX gitops reconcile '$GS' 2>&1) || { echo \"a passagem depois do resume falhou: \$out\"; exit 1; }
    grep -q 'in sync' <<<\"\$out\" || { echo \"depois do resume: \$out\"; exit 1; }
    [ \"\$(goapplied)\" = 'synced $GOC6 $GOC6' ] || { echo \"status: \$(goapplied)\"; exit 1; }"
  check "gitops: uma fonte inexistente diz 4 (DX-4601)" 4 \
    env HOME="$GO/home" XDG_CONFIG_HOME="$GO/home/.config" PATH="$GO/bin:$PATH" "$BIN" gitops reconcile "${PFX}-nao-existe"

  check "gitops: delete gitopssources desactiva o temporizador e apaga a fonte" ok bash -c "
    : >'$GO/systemctl.log'
    gox delete gitopssources '$GS' >/dev/null || exit 1
    grep -q 'disable --now delonix-gitops-default.$GS.timer' '$GO/systemctl.log' || { echo 'timer não desactivado'; exit 1; }
    test ! -e '$GO/home/.config/systemd/user/delonix-gitops-default.$GS.timer' || { echo 'unit ficou'; exit 1; }
    $GOX gitops status '$GS' >/dev/null 2>&1; rc=\$?
    [ \$rc -eq 0 ] && { echo 'a fonte ficou'; exit 1; }
    [ \$rc -eq 4 ] || { echo \"gitops status: rc=\$rc, não 4 (inexistente)\"; exit 1; }"
  cp "$GO/work/env/prod/app.yaml" "$GO/app-destroy.yaml"
  sed -i "s|image: ${IMG%%:*}:gitops-nao-existe|image: $IMG|" "$GO/app-destroy.yaml"
  gox stack destroy -f "$GO/app-destroy.yaml" --name "$GSS" >/dev/null 2>&1 || true
  gox stack destroy -f "$GO/source.yaml" --name "$GSB" >/dev/null 2>&1 || true
fi

########################################
section "métricas: /metrics expõe gauges por container (cpu/mem)"
########################################
# §14 "métricas recolhidas por scrape": o endpoint Prometheus `/metrics` da API
# de gestão (`serve api`, o irmão delonix-mgmt) expõe, para CADA container a
# correr, séries com as labels kind/id/name lidas do seu cgroup v2 vivo — entre
# elas o uso de CPU (`workload_cpu_usage_seconds_total`, de cpu.stat usage_usec,
# sempre presente num cgroup v2) e a memória corrente
# (`workload_memory_current_bytes`, de memory.current). Este check arranca um
# container, faz o SCRAPE real pelo socket e confirma que a série do container
# lá está, com o id e um valor de memória plausível; depois limpa tudo. É a
# prova viva por trás de MetricsPrometheus=supported (provider_report.rs).
MGMTSOCK="/tmp/dlxe2e-mgmt-$$.sock"
if [[ $E2E_HAVE_IMAGE -eq 1 ]] && command -v curl >/dev/null; then
  _mc="metrics-$PFX"
  if check_then "métricas: o container de prova arranca (container run -d)" ok \
       "$BIN" container run -d --name "$_mc" --net none "$IMG" sleep 120; then
    "$BIN" serve api --addr "unix://$MGMTSOCK" >"$OUT/mgmt-api.log" 2>&1 &
    MGMTPID=$!
    for _ in $(seq 100); do [[ -S "$MGMTSOCK" ]] && break; sleep 0.05; done
    check "métricas: o socket da API de gestão existe e é 0600" ok bash -c \
      "[[ -S '$MGMTSOCK' ]] && [[ \$(stat -c %a '$MGMTSOCK') == 600 ]]"
    check "métricas: GET /metrics devolve OpenMetrics com o registo partilhado" ok bash -c \
      "curl -s --unix-socket '$MGMTSOCK' http://localhost/metrics | grep -q '^# TYPE delonix_build_info '"
    check "métricas: há uma gauge de CPU e uma de memória com o id/nome do container e um valor plausível" ok env \
      MET="$(curl -s --unix-socket "$MGMTSOCK" http://localhost/metrics)" MCN="$_mc" \
      python3 -c '
import os,re
met=os.environ["MET"]; name=os.environ["MCN"]
cpu=re.search(r"delonix_workload_cpu_usage_seconds_total\{kind=\"container\",id=\"[^\"]+\",name=\""+re.escape(name)+r"\"\} ([0-9.eE+]+)", met)
mem=re.search(r"delonix_workload_memory_current_bytes\{kind=\"container\",id=\"[^\"]+\",name=\""+re.escape(name)+r"\"\} ([0-9.eE+]+)", met)
assert cpu, "sem workload_cpu_usage_seconds_total para "+name+"\n"+met
assert mem, "sem workload_memory_current_bytes para "+name+"\n"+met
assert float(mem.group(1))>0, "memory.current nao e plausivel: "+mem.group(1)
'
    kill "$MGMTPID" 2>/dev/null || true
  fi
  "$BIN" container rm -f "$_mc" >/dev/null 2>&1 || true
  rm -f "$MGMTSOCK"
  unset _mc
else
  skip "métricas: há uma gauge de CPU e uma de memória com o id/nome do container e um valor plausível" \
       "sem imagem base ($IMG) ou sem curl no host"
fi

########################################
section "métricas: /metrics expõe rx/tx de rede por container"
########################################
# §14 "metrics.network.per-workload": para CADA container com netns própria, o
# `/metrics` exporta duas séries COUNTER com as labels kind/id/name —
# `workload_network_receive_bytes_total` e `workload_network_transmit_bytes_total`
# — lidas de /proc/net/dev DENTRO da netns do container (todas as interfaces,
# não só eth0). Ao contrário das de cpu/mem (lidas inline no scrape), a rede
# precisa de um `nsenter` por container, caro demais no scrape, por isso é
# recolhida pelo refresh de fundo e servida da cache; o primeiro ciclo corre no
# arranque da API, daí o polling abaixo. Containers `--net host/none` não têm
# netns e NÃO aparecem (ausente != zero). Este check arranca um container COM
# rede, gera tráfego de dentro, faz o SCRAPE real e confirma as duas séries com
# o id/nome do container. É a prova viva por trás de
# MetricsPerWorkloadNetwork=supported (provider_report.rs).
MGMTSOCK="/tmp/dlxe2e-mgmtnet-$$.sock"
if [[ $E2E_HAVE_IMAGE -eq 1 ]] && command -v curl >/dev/null; then
  _mc="metricsnet-$PFX"
  _mnet="metricsnet-net-$PFX"
  # COM netns própria: attachado a uma bridge custom, ao contrário do default
  # (`net_mode: host`, sem netns) e do check de cpu/mem que usa --net none —
  # sem netns não há rx/tx por container para medir.
  "$BIN" network create "$_mnet" --subnet 10.231.0.0/16 >/dev/null 2>&1 || true
  if check_then "métricas rede: o container de prova arranca com rede (container run -d --net)" ok \
       "$BIN" container run -d --name "$_mc" --net "$_mnet" "$IMG" sleep 120; then
    # Tráfego para a série ter bytes plausíveis: pings à gateway de dentro da
    # netns (best-effort — a série existir já é a prova; os bytes reforçam-na).
    "$BIN" container exec "$_mc" sh -c \
      'GW=$(ip route 2>/dev/null | awk "/default/{print \$3; exit}"); [ -n "$GW" ] && ping -c 5 -W 1 "$GW" >/dev/null 2>&1 || true' \
      >/dev/null 2>&1 || true
    "$BIN" serve api --addr "unix://$MGMTSOCK" >"$OUT/mgmt-api-net.log" 2>&1 &
    MGMTPID=$!
    for _ in $(seq 100); do [[ -S "$MGMTSOCK" ]] && break; sleep 0.05; done
    # A cache de rede enche no 1º ciclo do refresh de fundo; sonda o /metrics até
    # a série aparecer (ou desiste ao fim de ~40s).
    for _ in $(seq 80); do
      curl -s --unix-socket "$MGMTSOCK" http://localhost/metrics \
        | grep -q "delonix_workload_network_receive_bytes_total{kind=\"container\",id=\"[^\"]*\",name=\"$_mc\"}" && break
      sleep 0.5
    done
    check "métricas rede: /metrics tem rx e tx por container com o id/nome e valor plausível" ok env \
      MET="$(curl -s --unix-socket "$MGMTSOCK" http://localhost/metrics)" MCN="$_mc" \
      python3 -c '
import os,re
met=os.environ["MET"]; name=os.environ["MCN"]
rx=re.search(r"delonix_workload_network_receive_bytes_total\{kind=\"container\",id=\"[^\"]+\",name=\""+re.escape(name)+r"\"\} ([0-9.eE+]+)", met)
tx=re.search(r"delonix_workload_network_transmit_bytes_total\{kind=\"container\",id=\"[^\"]+\",name=\""+re.escape(name)+r"\"\} ([0-9.eE+]+)", met)
assert rx, "sem workload_network_receive_bytes_total para "+name+"\n"+met
assert tx, "sem workload_network_transmit_bytes_total para "+name+"\n"+met
assert float(rx.group(1))>=0 and float(tx.group(1))>=0, "bytes negativos?!"
'
    kill "$MGMTPID" 2>/dev/null || true
  fi
  "$BIN" container rm -f "$_mc" >/dev/null 2>&1 || true
  "$BIN" network rm "$_mnet" >/dev/null 2>&1 || true
  rm -f "$MGMTSOCK"
  unset _mc _mnet
else
  skip "métricas rede: /metrics tem rx e tx por container com o id/nome e valor plausível" \
       "sem imagem base ($IMG) ou sem curl no host"
fi

########################################
section "cluster kind, ao vivo (E2E_CLUSTER=1)"
########################################
# Um cluster kind REAL de ponta a ponta: `cluster create` single-node a Ready, e
# a 1ª ligação cross-namespace de um pod NOVO no instante da prontidão (#736/#737).
# A lógica vive em `scripts/e2e_cluster_kind.sh`, que corre sozinho contra
# qualquer binário e sai 0/1/77 (ver o seu cabeçalho). FORA da bateria por
# omissão: o bring-up demora ~2m45s e um nó kind + imagem comem disco — só corre
# com `E2E_CLUSTER=1`. Corre contra o root REAL (onde a imagem `kindest/node`
# está em cache), nunca o root isolado desta corrida, e destrói o seu cluster.
_ckname="repro de #736: control-plane Ready e 1ª ligação cross-namespace (podIP + Service/DNS) no instante da prontidão de um pod novo"
if [[ "${E2E_CLUSTER:-0}" == "1" ]]; then
  if command -v kubectl >/dev/null; then
    _out=$(DELONIX_ROOT="$REAL_ROOT" bash "$(dirname "$0")/e2e_cluster_kind.sh" "$BIN" "ck$PFX" 2>&1); _rc=$?
    if [[ $_rc -eq 77 ]]; then
      skip "$_ckname" "$_out"
    else
      check "$_ckname" ok bash -c 'printf "%s\n" "$1"; exit "$2"' _ "$_out" "$_rc"
    fi
  else
    skip "$_ckname" "sem kubectl no host"
  fi
else
  skip "$_ckname" "E2E_CLUSTER!=1 (bring-up ~2m45s + disco; opcional por desenho)"
fi

########################################
section "guarda: o results.jsonl tem uma linha por veredicto"
########################################
# The totals below come from the shell counters; the ratchet and every reader
# after this run come from the JSONL. On 2026-10-21 they disagreed by one (a
# check whose python3 could not start, see `check`) and nothing said so. Every
# `check` writes its raw PASS/FAIL line, `xfail`/`xflaky` add an XFAIL/XPASS
# line after it, and `skip` writes a SKIP line — so the counts must match
# exactly, measured before this check adds its own line.
check "o results.jsonl tem uma linha por veredicto contado" ok python3 -I - \
  "$OUT/results.jsonl" "$((PASS + FAIL + XFAIL + XPASS))" "$SKIP" "$((XFAIL + XPASS))" <<'PY'
import collections, json, sys
path, raw, skip, marked = sys.argv[1], *map(int, sys.argv[2:5])
seen = collections.Counter()
for n, line in enumerate(open(path, encoding="utf-8"), 1):
    try:
        seen[json.loads(line)["verdict"]] += 1
    except (ValueError, KeyError) as e:
        sys.exit(f"line {n} is not a verdict: {e}")
got = (seen["PASS"] + seen["FAIL"], seen["SKIP"], seen["XFAIL"] + seen["XPASS"])
want = (raw, skip, marked)
if got != want:
    sys.exit(f"JSONL (PASS+FAIL, SKIP, XFAIL+XPASS) = {got}, counters = {want}")
PY

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
