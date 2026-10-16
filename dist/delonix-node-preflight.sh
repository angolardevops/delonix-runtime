#!/bin/sh
# delonix-node-preflight — verifica, num nó, os pré-requisitos de rede de um
# perfil de CNI. Instalado pela imagem dourada em /usr/local/bin e corrido pelo
# `delonix cluster apply`/`cluster kubeadm` ANTES do `kubeadm init`/`join`.
#
# PORQUE EXISTE. A golden passou a persistir módulos e sysctls (ver
# `k8s_recipes::cni_prereq_recipes`), mas há pré-requisitos que ela
# deliberadamente NÃO configura — o bpffs e a hierarquia unificada de cgroup já
# são verdade por omissão nestas distribuições, e escrever por cima de um
# sistema que funciona para satisfazer uma checklist é a maneira de partir o que
# não era preciso tocar. O que não se configura, verifica-se: uma verificação
# que falha alto vale mais do que uma alteração que não fez nada em silêncio.
#
# CONTRATO. Um argumento: o nome do perfil (cilium|calico|flannel|default|none).
# Imprime uma linha por verificação e sai com:
#   0  tudo o que este perfil precisa está presente
#   1  falta algo que este perfil precisa (as linhas FAIL dizem o quê)
#   2  uso incorrecto
# NUNCA sai 0 por não ter conseguido verificar: um «não sei» conta como falha,
# porque um preflight que se cala quando não consegue medir é pior do que
# nenhum.
#
# Reporta TODAS as falhas, não para na primeira — a mesma razão do
# `scripts/lab_preflight.sh` deste repositório: um nó a que falta uma coisa é a
# mesma mentira um nível acima quando só se nomeia a primeira.

set -u

PROFILE="${1:-}"
case "$PROFILE" in
    cilium | calico | flannel | default | none) ;;
    *)
        echo "uso: $0 <cilium|calico|flannel|default|none>" >&2
        exit 2
        ;;
esac

fails=0

ok()   { echo "OK    $1"; }
fail() { echo "FAIL  $1"; fails=$((fails + 1)); }
skip() { echo "SKIP  $1"; }

# --- comum a qualquer nó kubeadm -------------------------------------------

if [ "$(id -u)" -ne 0 ]; then
    # Sem root não se lê /proc/sys com fiabilidade nem se vê o cgroup todo. Dizer
    # «não sei» e sair 1, nunca passar por omissão.
    fail "este preflight precisa de root para medir (correu como uid $(id -u))"
    echo "$fails verificação(ões) falhada(s)"
    exit 1
fi

if swapon --show 2>/dev/null | grep -q .; then
    fail "swap activa — o kubelet recusa arrancar com swap (ver a receita 'swap desligado')"
else
    ok "swap desligada"
fi

for m in overlay br_netfilter; do
    if lsmod 2>/dev/null | grep -q "^$m" || [ -d "/sys/module/$m" ]; then
        ok "módulo $m presente"
    else
        fail "módulo $m ausente — o kubelet e o CNI precisam dele"
    fi
done

for s in net.ipv4.ip_forward net.bridge.bridge-nf-call-iptables; do
    v="$(sysctl -n "$s" 2>/dev/null || echo "")"
    if [ "$v" = "1" ]; then
        ok "$s=1"
    else
        fail "$s=${v:-ausente} (tem de ser 1)"
    fi
done

# A ÚNICA coisa que o CRI deste motor exige e que mais ninguém verifica: o
# socket a que o kubeadm vai falar. Um nó sem ele falha no preflight do kubeadm
# com uma mensagem sobre containerd, que manda o leitor para o lado errado.
if systemctl is-active --quiet delonix-cri 2>/dev/null; then
    ok "delonix-cri activo"
else
    fail "delonix-cri não está activo — o kubeadm não tem CRI a que falar"
fi
if [ -S /run/delonix-cri.sock ]; then
    ok "socket /run/delonix-cri.sock presente"
else
    fail "socket /run/delonix-cri.sock ausente"
fi

# --- plugins CNI de referência ---------------------------------------------
# Quais, depende do perfil: um binário em falta que o perfil nunca executa não
# é motivo para recusar o nó.
case "$PROFILE" in
    cilium)  need_plugins="loopback portmap" ;;
    calico)  need_plugins="loopback portmap bandwidth" ;;
    flannel) need_plugins="loopback bridge host-local portmap" ;;
    default) need_plugins="loopback bridge host-local portmap" ;;
    none)    need_plugins="" ;;
esac
for p in $need_plugins; do
    if [ -x "/opt/cni/bin/$p" ]; then
        ok "plugin /opt/cni/bin/$p"
    else
        fail "plugin /opt/cni/bin/$p ausente — a cadeia deste perfil executa-o"
    fi
done

# --- encapsulamento ---------------------------------------------------------
# Sem o módulo, a interface de túnel não nasce e o tráfego entre nós não sai do
# nó — a falha lê-se como «pods do mesmo nó falam, de nós diferentes não».
case "$PROFILE" in
    cilium | calico | flannel)
        if modprobe -n vxlan 2>/dev/null || [ -d /sys/module/vxlan ]; then
            ok "vxlan disponível"
        else
            fail "vxlan indisponível — sem ele não há tráfego de pods entre nós"
        fi
        ;;
    *) skip "encapsulamento (o perfil $PROFILE não encapsula)" ;;
esac

# --- específico do Cilium ---------------------------------------------------
if [ "$PROFILE" = "cilium" ]; then
    # bpffs: o agente guarda aqui os mapas eBPF. O systemd monta-o por omissão
    # nestas distribuições — é precisamente por isso que a golden NÃO o escreve
    # e que se verifica aqui.
    if mount 2>/dev/null | grep -q "on /sys/fs/bpf type bpf"; then
        ok "bpffs montado em /sys/fs/bpf"
    else
        fail "/sys/fs/bpf não está montado como bpf — o agente Cilium não consegue fixar os mapas eBPF"
    fi
    # cgroup v2 unificado: o Cilium liga programas eBPF ao cgroup root.
    if [ -f /sys/fs/cgroup/cgroup.controllers ]; then
        ok "cgroup v2 (hierarquia unificada)"
    else
        fail "cgroup v2 unificado ausente — o Cilium liga programas eBPF ao cgroup root"
    fi
    # O kernel. 5.10 é o mínimo que a documentação do Cilium declara; abaixo
    # disso o dataplane dele não arranca.
    kv="$(uname -r)"
    kmaj="${kv%%.*}"
    krest="${kv#*.}"
    kmin="${krest%%.*}"
    if [ "${kmaj:-0}" -gt 5 ] 2>/dev/null ||
        { [ "${kmaj:-0}" -eq 5 ] 2>/dev/null && [ "${kmin:-0}" -ge 10 ] 2>/dev/null; }; then
        ok "kernel $kv (>= 5.10)"
    else
        fail "kernel $kv é anterior a 5.10, o mínimo do dataplane eBPF do Cilium"
    fi
fi

# --- específico do Calico ---------------------------------------------------
if [ "$PROFILE" = "calico" ]; then
    # O calico-node recusa arrancar com rp_filter=1 nas interfaces dos pods.
    v="$(sysctl -n net.ipv4.conf.all.rp_filter 2>/dev/null || echo "")"
    if [ "$v" = "0" ] || [ "$v" = "2" ]; then
        ok "net.ipv4.conf.all.rp_filter=$v"
    else
        fail "net.ipv4.conf.all.rp_filter=${v:-ausente} — o calico-node recusa arrancar com 1"
    fi
    for m in ip_tables nf_conntrack; do
        if lsmod 2>/dev/null | grep -q "^$m" || [ -d "/sys/module/$m" ]; then
            ok "módulo $m presente"
        else
            fail "módulo $m ausente — o Calico programa iptables directamente"
        fi
    done
fi

# --- diagnóstico ------------------------------------------------------------
# Não falha o nó: é o que permite DIAGNOSTICAR, não o que faz a rede funcionar.
for t in ip ss tcpdump ethtool conntrack; do
    if command -v "$t" >/dev/null 2>&1; then
        ok "ferramenta de diagnóstico $t"
    else
        skip "ferramenta de diagnóstico $t ausente (não impede a rede; dificulta o diagnóstico)"
    fi
done

if [ "$fails" -eq 0 ]; then
    echo "preflight do perfil $PROFILE: tudo presente"
    exit 0
fi
echo "preflight do perfil $PROFILE: $fails verificação(ões) falhada(s)"
exit 1
