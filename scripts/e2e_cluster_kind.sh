#!/usr/bin/env bash
# E2E REAL de um cluster `kind` single-node, de ponta a ponta: `delonix cluster
# create` arranca um `kindest/node` no próprio motor, espera o control-plane a
# `Ready`, e logo no INSTANTE da prontidão de um pod NOVO dispara a 1ª ligação
# cross-namespace — o repro de #736/#737.
#
#   e2e_cluster_kind.sh <binário> [prefixo]
#
# Chamado pelo `scripts/e2e.sh` (secção «cluster kind, ao vivo»), SÓ quando
# `E2E_CLUSTER=1`: o bring-up demora ~2m45s e um nó kind + imagem comem disco,
# por isso fica fora da bateria rápida por omissão. Corre sozinho contra
# qualquer binário; usa o `kubectl` do host. Sai 0 (certo), 1 (repro de #736 —
# 1º pacote perdido, com a medição na saída) ou 77 (o host não deixa medir:
# nem o cluster sobe, nem o caminho morno responde).
#
# ## Porquê este desenho, e o que cada número quer dizer
#
# #736 («pod-to-pod loss e a 1ª ligação cross-namespace») e #737 (a corrida
# veth/ARP/conntrack na criação do pod) são sobre o PRIMEIRO pacote de um pod
# acabado de nascer: antes de o ARP para o gateway resolver, de o conntrack ter
# entrada, de o kube-proxy ter programado o ClusterIP novo. Ligar DEPOIS de um
# `sleep` de acomodação passa ao lado dessa janela — foi por isso que o caminho
# ingénuo (medido a 2026-10-22) nunca reproduziu. Aqui NÃO há `sleep`: o
# `kubectl exec` vem encadeado (`&&`) logo a seguir ao `kubectl wait
# --for=condition=Ready`, e a primeira coisa que o pod faz na rede é a ligação
# que se mede.
#
# Cada cliente é um pod NOVO (`alpine`, carregado no nó — arranque sem pull, o
# tempo até `Ready` é quase só o do CNI a montar o veth), e dispara UMA tentativa
# curta. O destino é um servidor já `Ready` e MORNO (aquecido a partir de um pod
# de longa vida antes de os clientes nascerem), para que o que se meça seja o
# 1º pacote DO CLIENTE e não um servidor frio.
#
# O discriminador entre «1º pacote perdido» e ruído:
#   - tentativa curta OK                      -> FIRSTOK   (sem defeito)
#   - curta sem resposta (timeout) e depois o
#     caminho morno (timeout generoso) responde -> FIRSTLOST (reproduz #736) -> 1
#   - «connection refused»/«reset»             -> janela de reabertura do `nc`
#     ou ClusterIP ainda por programar: NÃO é perda, re-tenta a curta
#   - nem a curta nem o morno respondem         -> UNREACH  (rede do cluster
#     partida — não é medição limpa) -> 77
# O SYN de um 1º pacote perdido fica sem resposta (timeout), ao passo que uma
# janela do servidor dá RST imediato («refused»): são sinais distintos, e é isso
# que deixa esta medição acusar #736 sem falsos positivos do servidor.
set -u

BIN="${1:?binário}"; PFX="${2:-ck$$}"
# Nomes DNS-1123 (minúsculas, alfanum + '-'): $$ são dígitos, serve.
CLUSTER="e2ekind-$PFX"
NSA="nsa-$PFX"            # cliente (namespace A)
NSB="nsb-$PFX"            # servidor (namespace B) — cross-namespace = A -> B
IMG="alpine:latest"       # no store local (3.7 MiB); carregado no nó
PORT=8080

ROOT="${DELONIX_ROOT:-${XDG_DATA_HOME:-$HOME/.local/share}/delonix}"
KC="$ROOT/clusters/$CLUSTER-kubeconfig.yaml"

k() { KUBECONFIG="$KC" kubectl "$@"; }

destroy() {
  # --force é obrigatório fora de um TTY; repete até o `cluster ls` largar o nome.
  local _
  for _ in 1 2 3 4 5; do
    timeout 180 "$BIN" cluster destroy "$CLUSTER" --force >/dev/null 2>&1
    "$BIN" cluster ls 2>/dev/null | grep -qw "$CLUSTER" || return 0
    sleep 3
  done
}
cleanup() { destroy; }
trap cleanup EXIT

die()    { echo "$*"; exit 1; }    # defeito (repro de #736)
cannot() { echo "$*"; exit 77; }   # o host não deixa medir

# --- bring-up ----------------------------------------------------------------
# `cluster create` (modo kind) só volta DEPOIS de o control-plane estar Ready
# (kindmode.rs: «Waiting for control-plane to be Ready»). ~2m45s neste host.
out=$(timeout 400 "$BIN" cluster create --name "$CLUSTER" --workers 0 2>&1); rc=$?
[[ $rc -eq 124 ]] && cannot "cluster create não voltou em 400s (disco saturado?): $(tail -c 300 <<<"$out")"
[[ $rc -eq 0 ]]   || cannot "cluster create falhou (rc=$rc) — o host não arranca um nó kind: $(tail -c 500 <<<"$out")"
[[ -f "$KC" ]]    || cannot "cluster create voltou 0 mas não há kubeconfig em $KC"

# Critério 1: o control-plane está Ready, lido pelo kubeconfig cacheado.
nodes=$(k get nodes --no-headers 2>&1) || cannot "kubectl get nodes falhou pelo kubeconfig cacheado: $nodes"
grep -qw Ready <<<"$nodes" || cannot "o control-plane não está Ready: $nodes"
echo "READY: $(tr -s ' ' <<<"$nodes" | tr '\n' ';')"

# A imagem do cliente no nó, sem registry (arranque sem pull -> o tempo até
# Ready é quase só o CNI). Se o load falhar, IfNotPresent deixa o nó puxar.
timeout 120 "$BIN" cluster load "$IMG" --name "$CLUSTER" >/dev/null 2>&1 || true

# --- servidor cross-namespace (ns B), contínuo e morno -----------------------
# Servidor HTTP minimalista em busybox `nc`, num laço: responde 200/«ok» a cada
# ligação. A janela entre reaberturas dá RST (não timeout), e o cliente sabe
# distingui-la de um pacote perdido.
k apply -f - >/dev/null <<YAML || cannot "kubectl apply (servidor) falhou"
apiVersion: v1
kind: Namespace
metadata: { name: $NSB }
---
apiVersion: v1
kind: Namespace
metadata: { name: $NSA }
---
apiVersion: v1
kind: Pod
metadata: { name: srv, namespace: $NSB, labels: { app: srv } }
spec:
  containers:
  - name: srv
    image: $IMG
    imagePullPolicy: IfNotPresent
    command: ["sh","-c","while true; do printf 'HTTP/1.0 200 OK\r\nContent-Length: 2\r\n\r\nok' | nc -l -p $PORT; done"]
  terminationGracePeriodSeconds: 1
---
apiVersion: v1
kind: Service
metadata: { name: srv, namespace: $NSB }
spec:
  selector: { app: srv }
  ports: [{ port: $PORT, targetPort: $PORT }]
YAML

k wait --for=condition=Ready pod/srv -n "$NSB" --timeout=120s >/dev/null 2>&1 \
  || cannot "o servidor não ficou Ready: $(k describe pod/srv -n "$NSB" 2>&1 | tail -5)"
SRVIP=$(k get pod srv -n "$NSB" -o jsonpath='{.status.podIP}' 2>/dev/null)
[[ -n "$SRVIP" ]] || cannot "o servidor não tem podIP"

# Aquece o servidor e o CoreDNS a partir de um pod de longa vida em ns A, para
# que o 1º pacote dos clientes meça o lado DELES, não um servidor/DNS frios.
k run warmer -n "$NSA" --image="$IMG" --image-pull-policy=IfNotPresent \
  --restart=Never --command -- sleep 3600 >/dev/null 2>&1
k wait --for=condition=Ready pod/warmer -n "$NSA" --timeout=120s >/dev/null 2>&1 \
  || cannot "o pod de aquecimento não ficou Ready"
SVCFQDN="srv.$NSB.svc.cluster.local"
k exec -n "$NSA" warmer -- sh -c \
  "for i in \$(seq 1 20); do wget -T 5 -q -O /dev/null http://$SRVIP:$PORT/ && wget -T 5 -q -O /dev/null http://$SVCFQDN:$PORT/ && break; sleep 1; done" \
  >/dev/null 2>&1 || cannot "nem o caminho morno (podIP+svc) responde — a rede do cluster está partida"

# --- o repro: 1º pacote de um pod NOVO, no instante de Ready -----------------
# Corre DENTRO do pod: tentativa curta; classifica; cai para o morno. `timeout`
# limita o connect (um SYN perdido retransmite durante segundos no kernel).
read -r -d '' PROBE <<'SH' || true
url=$1
for shot in 1 2 3 4 5; do
  out=$(timeout 3 wget -T 3 -O /dev/null "$url" 2>&1); rc=$?
  if [ "$rc" = 0 ]; then echo FIRSTOK; exit 0; fi
  case "$out" in
    *refused*|*reset*)            # RST: janela do nc / ClusterIP por programar
      continue ;;
    *)                            # sem resposta (timeout): assinatura de perda
      w=1
      while [ "$w" -le 12 ]; do
        if timeout 8 wget -T 8 -O /dev/null "$url" 2>/dev/null; then echo FIRSTLOST; exit 0; fi
        w=$((w+1))
      done
      echo UNREACH; exit 0 ;;
  esac
done
echo GAPONLY   # 5 tentativas só com RST — inconclusivo, não é perda
SH

# probe_fresh <rótulo> <nome-pod> <url> -> ecoa o veredicto do pod
probe_fresh() {
  local label=$1 pod=$2 url=$3
  k apply -f - >/dev/null 2>&1 <<YAML || { echo "$label: apply falhou"; echo UNREACH; return; }
apiVersion: v1
kind: Pod
metadata: { name: $pod, namespace: $NSA }
spec:
  containers:
  - name: c
    image: $IMG
    imagePullPolicy: IfNotPresent
    command: ["sleep","3600"]
  terminationGracePeriodSeconds: 1
YAML
  # SEM sleep: o exec vem encadeado ao wait. A 1ª rede do pod é esta ligação.
  if ! k wait --for=condition=Ready "pod/$pod" -n "$NSA" --timeout=120s >/dev/null 2>&1; then
    echo "$label: o pod cliente não ficou Ready"; echo UNREACH; return
  fi
  k exec -n "$NSA" "$pod" -- sh -c "$PROBE" _ "$url" 2>/dev/null
}

# Dois clientes NOVOS, um por forma da «1ª ligação cross-namespace»:
#   podip  — plano de dados puro (CNI): SYN do cliente -> veth/ARP/conntrack (#737)
#   svcdns — a forma do reporter: 1º DNS (UDP ao CoreDNS) + ClusterIP cross-ns (#736)
V_PODIP=$(probe_fresh podip  cli-ip  "http://$SRVIP:$PORT/")
V_SVC=$(probe_fresh   svcdns cli-dns "http://$SVCFQDN:$PORT/")
echo "PODIP=$V_PODIP SVCDNS=$V_SVC"

# --- veredicto ---------------------------------------------------------------
case "$V_PODIP|$V_SVC" in
  FIRSTLOST*|*FIRSTLOST) die "repro de #736: 1º pacote perdido (podip=$V_PODIP svcdns=$V_SVC)" ;;
esac
case "$V_PODIP" in FIRSTOK) ;; *) cannot "podip inconclusivo ($V_PODIP) — não é medição limpa" ;; esac
case "$V_SVC"   in FIRSTOK) ;; *) cannot "svcdns inconclusivo ($V_SVC) — não é medição limpa" ;; esac

echo "OK: control-plane Ready e 1ª ligação cross-namespace (podIP e Service/DNS) entregou o 1º pacote"
exit 0
