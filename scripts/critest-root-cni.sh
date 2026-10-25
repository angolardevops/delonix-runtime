#!/usr/bin/env bash
# Conformância CRI na via ROOT + CNI, num ambiente com IP de pod ROTEÁVEL.
#
# Porquê: specs como `portforward [Conformance]` (netns normal), `networking` e
# `port mapping` alcançam o IP do pod DIRECTAMENTE do processo de teste. Num host
# rootless (slirp/pasta) esse IP não é roteável e eles falham no PRÉ-REQUISITO,
# antes de exercitarem o código em foco. Para os exercitar a sério é preciso a via
# ROOT + CNI (bridge real + veth + IPAM), onde o host routa para o pod.
#
# Como, sem root no host: corre o `delonix-cri` como root DENTRO de um container
# `--privileged` (basta o grupo `docker`). Lá dentro ele toma a via root/CNI e os
# IPs de pod são roteáveis na netns do container; o `critest` corre ao lado.
#
# Uso:   ./scripts/critest-root-cni.sh [FOCUS-regex]   (default: portforward \[Conformance\])
# Env:   GINKGO_V=1   → -ginkgo.v (timeline por-passo, mesmo em PASS)
#        BASE=ubuntu:24.04   (bate o glibc do host; muda se o teu host diferir)
#        KEEP=1       → não remove o container/volume no fim
set -uo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
PROFILE="${PROFILE:-debug}"
BIN_DIR="$ROOT_DIR/target/$PROFILE"
BASE="${BASE:-ubuntu:24.04}"
FOCUS="${1:-portforward \\[Conformance\\]}"
NAME="pfw-root-cni-$$"
VOL="pfw-root-cni-$$"

command -v docker >/dev/null || { echo "ERRO: docker não está no PATH"; exit 2; }
command -v critest >/dev/null || { echo "ERRO: critest não está no PATH"; exit 2; }
command -v crictl  >/dev/null || { echo "ERRO: crictl não está no PATH"; exit 2; }
[[ -d /opt/cni/bin ]] || { echo "ERRO: faltam os plugins CNI em /opt/cni/bin"; exit 2; }

# Build (idempotente) dos DOIS binários — o CRI delega o ciclo de vida na CLI.
if [[ "${NO_BUILD:-}" != "1" ]]; then
  echo "==> cargo build -p delonix-cri -p delonix-runtime-bin ($PROFILE)"
  ( cd "$ROOT_DIR" && cargo build ${PROFILE:+$([[ $PROFILE == release ]] && echo --release)} \
      -p delonix-cri --bin delonix-cri -p delonix-runtime-bin --bin delonix ) || exit $?
fi
[[ -x "$BIN_DIR/delonix-cri" && -x "$BIN_DIR/delonix" ]] || { echo "ERRO: binários em falta em $BIN_DIR"; exit 2; }

WORK="$(mktemp -d /tmp/pfw-root-cni.XXXXXX)"
trap 'rm -rf "$WORK"; [[ "${KEEP:-}" == "1" ]] || { docker rm -f "$NAME" >/dev/null 2>&1; docker volume rm "$VOL" >/dev/null 2>&1; }' EXIT

# Conflist: bridge + host-local IPAM (IP de pod roteável na netns do container) +
# portmap (hostPort, que o spec usa).
cat > "$WORK/10-delonix.conflist" <<'JSON'
{
  "cniVersion": "1.0.0",
  "name": "delonixnet",
  "plugins": [
    { "type": "bridge", "bridge": "cni0", "isGateway": true, "ipMasq": true, "hairpinMode": true,
      "ipam": { "type": "host-local", "ranges": [[{ "subnet": "10.88.0.0/16" }]], "routes": [{ "dst": "0.0.0.0/0" }] } },
    { "type": "portmap", "capabilities": { "portMappings": true } }
  ]
}
JSON

# Entrypoint que corre DENTRO do container (root).
cat > "$WORK/entrypoint.sh" <<'EOS'
#!/usr/bin/env bash
set -uo pipefail
export PATH=/opt/bin:/usr/local/bin:$PATH
export CNI_PATH=/opt/cni/bin DELONIX_CNI=1 DELONIX_ROOT=/run/dlx
export DELONIX_CRI_ADDR=unix:///run/cri.sock DELONIX_CRI_CAP_CEILING=all RUST_MIN_STACK=16777216
FOCUS="${FOCUS:-portforward \\[Conformance\\]}"
mkdir -p /run/netns /etc/cni/net.d /run/dlx
cp /opt/conf/10-delonix.conflist /etc/cni/net.d/

echo "==> iproute2 + iptables (o CNI precisa de ip/iptables)"
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq >/tmp/apt.log 2>&1 && apt-get install -y -qq iproute2 iptables >>/tmp/apt.log 2>&1 \
  || { echo "ERRO apt"; tail -20 /tmp/apt.log; exit 4; }

# Delegação de cgroup v2 p/ runtime aninhado: tira os nossos processos da raiz da
# cgroup-ns (regra "no internal processes") e habilita os controladores, p/ que
# delonix.slice receba `memory`/`cpu`/`io`/`pids` — senão o container start falha
# com "memory controller is not delegated to /sys/fs/cgroup/delonix.slice/...".
if [ -w /sys/fs/cgroup/cgroup.subtree_control ]; then
  mkdir -p /sys/fs/cgroup/init
  echo $$ > /sys/fs/cgroup/init/cgroup.procs 2>/dev/null || true
  echo "+cpu +memory +io +pids" > /sys/fs/cgroup/cgroup.subtree_control 2>/dev/null || true
fi

echo "==> identidade (espera uid=0)"; id
setsid delonix-cri >/tmp/cri.server.log 2>&1 < /dev/null &
for _ in $(seq 1 60); do [ -S /run/cri.sock ] && break; sleep 0.3; done
[ -S /run/cri.sock ] || { echo "ERRO: socket não abriu"; cat /tmp/cri.server.log; exit 3; }
crictl --runtime-endpoint unix:///run/cri.sock version >/dev/null || { cat /tmp/cri.server.log; exit 3; }

echo "==> critest focus='$FOCUS'"
critest -runtime-endpoint unix:///run/cri.sock -image-endpoint unix:///run/cri.sock \
  -ginkgo.timeout=20m -ginkgo.no-color ${GINKGO_V:+-ginkgo.v} -ginkgo.focus="$FOCUS" 2>&1
rc=$?
echo "RC=$rc"
echo "=== delonix-cri server log (tail) ==="; tail -40 /tmp/cri.server.log
exit $rc
EOS

echo "==> container ($BASE, privileged) — root+CNI, IP de pod roteável"
docker volume create "$VOL" >/dev/null
docker run --rm --name "$NAME" --privileged \
  --security-opt seccomp=unconfined --security-opt apparmor=unconfined \
  -e "FOCUS=$FOCUS" -e "GINKGO_V=${GINKGO_V:-}" \
  -v "$VOL:/run/dlx" \
  -v "$BIN_DIR/delonix:/opt/bin/delonix:ro" \
  -v "$BIN_DIR/delonix-cri:/opt/bin/delonix-cri:ro" \
  -v "$(command -v critest):/usr/local/bin/critest:ro" \
  -v "$(command -v crictl):/usr/local/bin/crictl:ro" \
  -v "/opt/cni/bin:/opt/cni/bin:ro" \
  -v "$WORK/10-delonix.conflist:/opt/conf/10-delonix.conflist:ro" \
  -v "$WORK/entrypoint.sh:/opt/conf/entrypoint.sh:ro" \
  "$BASE" bash /opt/conf/entrypoint.sh
rc=$?
echo "==> terminou com rc=$rc"
exit $rc
