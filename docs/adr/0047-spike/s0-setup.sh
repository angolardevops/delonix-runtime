#!/usr/bin/env bash
# S0 — the bench: two networks, two namespaces, one backend, clients in every
# combination, and a manual HTTPRoute on port $PORT to the backend.
set -u
. "$(dirname "$0")/env.sh"
"$B" --version | head -1
"$B" network create netA >/dev/null && echo "netA created"
"$B" network create netB >/dev/null && echo "netB created"
"$B" image pull alpine:3.20 >/dev/null 2>&1 && echo "alpine:3.20 pulled"
run() { # run <name> <ns> <net> <cmd...>
  local n=$1 ns=$2 net=$3; shift 3
  "$B" container run -d --name "$n" --namespace "$ns" --net "$net" alpine:3.20 "$@" >/dev/null \
    && echo "$n ns=$ns net=$net ip=$(ipof "$n")"
}
# The backend: a busybox `nc -lk -e` server on 8080 answering with its own name
# (alpine's busybox has no `httpd` applet — it is in busybox-extras).
run be-a teamA netA sh -c \
  'printf "#!/bin/sh\nprintf \"HTTP/1.0 200 OK\\\\r\\\\nContent-Length: 9\\\\r\\\\nConnection: close\\\\r\\\\n\\\\r\\\\nWIN-be-a\\\\n\"\n" >/h.sh && chmod +x /h.sh && exec nc -lk -p 8080 -e /h.sh'
run cl-a  teamA netA sleep 3600   # same network, same namespace
run cl-b  teamB netA sleep 3600   # same network, other namespace
run cl-a2 teamA netB sleep 3600   # other network, same namespace
run cl-b2 teamB netB sleep 3600   # other network, other namespace
run cl-m  teamB netB sleep 3600   # will also join netA (multi-homed)
"$B" network connect netA cl-m && echo "cl-m joined netA"
# A pod in teamB on netA: its members share the pod netns and its address.
cat >"$DELONIX_ROOT/pod.yaml" <<YAML
apiVersion: compute.delonix.io/v1alpha1
kind: Pod
metadata: { name: po, namespace: teamB }
spec:
  network: netA
  containers:
    - { name: c0, image: "alpine:3.20", command: ["sleep", "3600"] }
YAML
"$B" pod create -f "$DELONIX_ROOT/pod.yaml" 2>&1 | tail -1
cat >"$DELONIX_ROOT/route.yaml" <<YAML
apiVersion: gateway.delonix.io/v1alpha1
kind: HTTPRoute
metadata: { name: app }
spec:
  entrypoints: [ { port: $PORT } ]
  rules:
    - host: app.test
      paths: [ { path: /, backend: { service: be-a, port: 8080 } } ]
YAML
"$B" net httproute apply -f "$DELONIX_ROOT/route.yaml" 2>&1 | tail -2
echo "## be-a answers itself: $("$B" container exec be-a wget -q -T 4 -O - http://127.0.0.1:8080/ 2>&1)"
echo "holder pid: $(cat "$DELONIX_ROOT/ingress/holder.pid")"
HN ip -4 -o addr | awk '{print "  holder", $2, $4}'
