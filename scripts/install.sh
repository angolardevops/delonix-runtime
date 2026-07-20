#!/usr/bin/env bash
# install.sh — instalador oficial do Delonix Runtime.
#
#   curl -fsSL https://github.com/angolardevops/delonix-runtime/releases/latest/download/install.sh | bash
#
# Objectivo: um utilizador SEM experiência de sysadmin acaba com uma instalação
# 100% funcional — binário + TODAS as dependências de runtime (containers E VMs)
# + a configuração de host que o modo rootless exige (subuid/subgid, AppArmor).
# Nada de passos manuais nem workarounds; tudo o que o motor invoca em runtime
# é instalado pelo gestor de pacotes da distro.
#
# Distros suportadas (detecção por ID/ID_LIKE do /etc/os-release):
#   apt    — Debian, Ubuntu e derivados (Mint, Zorin, Pop!_OS, ...)
#   dnf    — Fedora, RHEL, CentOS Stream, Rocky, AlmaLinux
#   zypper — openSUSE Leap/Tumbleweed, SLES
#   pacman — Arch, Manjaro, EndeavourOS
#
# Flags:
#   --no-vm        não instala as dependências de VMs (libvirt/qemu/cloud-init)
#   --no-tune      não aplica o tuning de kernel (sysctls/módulos)
#   --no-binary    só dependências/configuração (usa um binário já instalado)
#   --with-cri     instala também o delonix-cri (nó Kubernetes)
#   --user         binário em ~/.local/bin em vez de /usr/local/bin
#   --version vX   versão específica (default: latest)
#
# Porquê cada dependência (a lição veio de instalações reais que falharam):
#   slirp4netns   — rede user-mode; sem ele, `run -p` morre com ENOENT.
#   uidmap        — newuidmap/newgidmap (setuid); sem eles o userns só mapeia
#                   1 uid e qualquer imagem com utilizador não-root (nginx,
#                   postgres, ...) morre em chown() com EINVAL.
#   nftables      — firewall/DNAT da SDN (`nft -f -`).
#   iproute2      — `ip` (veth/bridge/netns da SDN).
#   conntrack     — limpeza de ligações ao despublicar portas.
#   VMs: libvirt+qemu (backend de VM; cloud-hypervisor onde empacotado),
#   qemu-img (discos overlay), cloud-localds (seed ISO do cloud-init).
set -euo pipefail

REPO="angolardevops/delonix-runtime"
VERSION="latest"
WITH_VM=1
WITH_TUNE=1
WITH_BINARY=1
WITH_CRI=0
USER_INSTALL=0

# `command -v` falha para binários de admin (/usr/sbin) quando o PATH do
# utilizador não os inclui (Debian) — mas o delonix invoca-os pelo PATH do
# processo, e o dos serviços/sudo inclui sbin. Procurar lá também.
has_cmd() { command -v "$1" >/dev/null 2>&1 || [ -x "/usr/sbin/$1" ] || [ -x "/sbin/$1" ]; }

# Output na MESMA gramática do `delonix cluster apply` (ver cmd/cluster.rs):
#   install/delonix: a preparar o host...
#   [deps] slirp4netns: já satisfeito (SKIP)
#   [deps] uidmap: a instalar... OK
#   install/delonix: pronto
# Cores só nos estados (OK/SKIP/AVISO/ERRO); desligadas fora de um tty.
if [ -t 1 ]; then
  C_OK=$'\033[1;32m'; C_SKIP=$'\033[2m'; C_WARN=$'\033[1;33m'; C_ERR=$'\033[1;31m'; C_0=$'\033[0m'
else
  C_OK=""; C_SKIP=""; C_WARN=""; C_ERR=""; C_0=""
fi
msg()   { printf 'install/delonix: %s\n' "$*"; }
step()  { printf '[%s] %s: %s\n' "$1" "$2" "$3"; }                    # estado neutro
skip()  { printf '[%s] %s: %sjá satisfeito (SKIP)%s\n' "$1" "$2" "$C_SKIP" "$C_0"; }
stepok(){ printf '[%s] %s: %sOK%s\n' "$1" "$2" "$C_OK" "$C_0"; }
warn()  { printf '%sAVISO%s %s\n' "$C_WARN" "$C_0" "$*" >&2; }
die()   { printf '%sERRO%s %s\n' "$C_ERR" "$C_0" "$*" >&2; exit 1; }

while [ $# -gt 0 ]; do
  case "$1" in
    --no-vm)      WITH_VM=0 ;;
    --no-tune)    WITH_TUNE=0 ;;
    --no-binary)  WITH_BINARY=0 ;;
    --with-cri)   WITH_CRI=1 ;;
    --user)       USER_INSTALL=1 ;;
    --version)    shift; VERSION="${1:?--version requires an argument}" ;;
    -h|--help)    grep '^#' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) die "unknown flag: $1 (see --help)" ;;
  esac
  shift
done

# ---------------------------------------------------------------- pré-condições
[ "$(uname -s)" = Linux ] || die "Delonix Runtime is Linux-only."
ARCH=$(uname -m)
[ "$ARCH" = x86_64 ] || die "no prebuilt binary for $ARCH yet (only x86_64). Build from source: cargo build --release -p delonix-runtime-bin"

# O utilizador REAL (o script pode correr sob sudo já): é para ele que se
# configuram subuid/grupos, não para o root.
REAL_USER="${SUDO_USER:-$(id -un)}"
REAL_HOME=$(getent passwd "$REAL_USER" | cut -d: -f6)

if [ "$(id -u)" -eq 0 ]; then
  SUDO=""
else
  command -v sudo >/dev/null 2>&1 || die "this script needs root for packages/config — install sudo or run as root"
  SUDO="sudo"
  msg "alguns passos precisam de root — o sudo pode pedir a tua password"
  # Autentica JÁ: assim, um falhanço de pkg_install mais à frente significa
  # mesmo "pacote indisponível", nunca "sudo falhou em silêncio".
  sudo -v || die "autenticação sudo falhou — corre de novo e introduz a password, ou corre como root"
fi

# ------------------------------------------------------------ detecção de distro
# NUNCA fazer `source /etc/os-release` no shell principal: ele define VERSION/
# NAME/ID e esmagava as nossas variáveis (bug real da v0.4.0 — o instalador
# tentava descarregar a release "18.1", a versão do SO). Subshell isola tudo.
PKG=""
if [ -r /etc/os-release ]; then
  DISTRO_IDS=$(. /etc/os-release; echo "${ID:-} ${ID_LIKE:-}")
  DISTRO_NAME=$(. /etc/os-release; echo "${PRETTY_NAME:-unknown}")
else
  DISTRO_IDS=""
  DISTRO_NAME="unknown"
fi
case " $DISTRO_IDS " in
  *" debian "*|*" ubuntu "*) PKG=apt ;;
  *" fedora "*|*" rhel "*|*" centos "*) PKG=dnf ;;
  *" suse "*|*" opensuse "*|*" sles "*) PKG=zypper ;;
  *" arch "*) PKG=pacman ;;
esac
# Fallback: pelo gestor presente (distros que não declaram ID_LIKE útil).
if [ -z "$PKG" ]; then
  for m in apt-get dnf zypper pacman; do
    command -v "$m" >/dev/null 2>&1 && { PKG=${m%-get}; break; }
  done
fi
[ -n "$PKG" ] || die "unsupported distro (need apt, dnf, zypper or pacman). Deps to install manually: slirp4netns uidmap nftables iproute2 conntrack"
msg "a preparar o host ($DISTRO_NAME, gestor $PKG)..."

# ---------------------------------------------------- detecção de hardware
# Serve duas decisões concretas: (a) que variante do binário descarregar
# (x86-64-v3 quando o CPU tem AVX2 — Zen 2+/Haswell+); (b) avisos de
# capacidade (RAM/disco) ANTES de o utilizador bater neles em produção —
# a lição do kubelet a despejar por disk-pressure ficou aprendida.
CPU_MODEL=$(sed -n 's/^model name[^:]*: //p' /proc/cpuinfo | head -1)
NCPU=$(nproc 2>/dev/null || echo '?')
RAM_GB=$(awk '/MemTotal/ {printf "%d", $2/1048576}' /proc/meminfo 2>/dev/null || echo '?')
DISK_FREE_GB=$(df -k --output=avail "$REAL_HOME" 2>/dev/null | tail -1 | awk '{printf "%d", $1/1048576}')
GPU_INFO=""
if command -v lspci >/dev/null 2>&1; then
  GPU_INFO=$(lspci 2>/dev/null | grep -Ei 'vga|3d controller' \
    | sed -E 's/^[0-9a-f:.]+ +//; s/^(VGA compatible controller|3D controller): +//' \
    | paste -sd ';' - | sed 's/;/ · /g')
elif [ -d /sys/class/drm ] && ls /sys/class/drm/card[0-9] >/dev/null 2>&1; then
  GPU_INFO="present (install pciutils for details)"
fi
CPU_VARIANT=""
# x86-64-v3 = AVX2+BMI2+FMA. O teu binário genérico continua a ser o fallback.
if grep -qm1 avx2 /proc/cpuinfo && grep -qm1 bmi2 /proc/cpuinfo && grep -qm1 fma /proc/cpuinfo; then
  CPU_VARIANT="-v3"
fi
if [ -n "$CPU_VARIANT" ]; then VARIANT_LABEL="x86-64-v3 (AVX2)"; else VARIANT_LABEL="x86-64 baseline"; fi
step host cpu "${CPU_MODEL:-desconhecido} (${NCPU} cpus, $VARIANT_LABEL)"
step host recursos "${RAM_GB}GB RAM · ${DISK_FREE_GB:-?}GB livres em $REAL_HOME"
[ -n "$GPU_INFO" ] && step host gpu "$GPU_INFO"
[ "${RAM_GB:-8}" != '?' ] && [ "${RAM_GB:-8}" -lt 2 ] 2>/dev/null && warn "menos de 2GB de RAM — VMs ficam apertadas; containers OK"
[ -n "$DISK_FREE_GB" ] && [ "$DISK_FREE_GB" -lt 10 ] 2>/dev/null && warn "menos de 10GB livres — pulls de imagens e rootfs enchem o disco depressa (o kubelet despeja pods sob disk-pressure)"

PKG_UPDATED=0
pkg_install() {
  # Instala o 1.º candidato disponível de uma lista "a|b|c" (os nomes variam
  # entre distros e versões — tentar por ordem é mais robusto que uma tabela
  # rígida por VERSION_ID).
  local candidates="$1" c
  for c in ${candidates//|/ }; do
    case "$PKG" in
      apt)
        [ "$PKG_UPDATED" = 1 ] || { $SUDO apt-get update -qq || true; PKG_UPDATED=1; }
        if $SUDO env DEBIAN_FRONTEND=noninteractive apt-get install -y -qq "$c" >/dev/null 2>&1; then return 0; fi ;;
      dnf)
        if $SUDO dnf install -y -q "$c" >/dev/null 2>&1; then return 0; fi ;;
      zypper)
        if $SUDO zypper --non-interactive install --no-recommends "$c" >/dev/null 2>&1; then return 0; fi ;;
      pacman)
        [ "$PKG_UPDATED" = 1 ] || { $SUDO pacman -Sy --noconfirm >/dev/null 2>&1 || true; PKG_UPDATED=1; }
        if $SUDO pacman -S --noconfirm --needed "$c" >/dev/null 2>&1; then return 0; fi ;;
    esac
  done
  return 1
}

require_dep() {
  # $1 = fase; $2 = comando que tem de existir no fim; $3 = candidatos; $4 = razão
  local phase="$1" cmd="$2" pkgs="$3" why="$4"
  if has_cmd "$cmd"; then skip "$phase" "$cmd"; return 0; fi
  step "$phase" "$cmd" "a instalar ($why)..."
  pkg_install "$pkgs" || die "não consegui instalar '$pkgs' — instala com o teu gestor de pacotes e volta a correr"
  has_cmd "$cmd" || die "'$pkgs' instalado mas '$cmd' continua ausente"
  stepok "$phase" "$cmd"
}

optional_dep() {
  local phase="$1" cmd="$2" pkgs="$3" why="$4"
  if has_cmd "$cmd"; then skip "$phase" "$cmd"; return 0; fi
  step "$phase" "$cmd" "a instalar ($why)..."
  if pkg_install "$pkgs" && has_cmd "$cmd"; then
    stepok "$phase" "$cmd"
  else
    warn "$cmd indisponível nesta distro — $why não vai funcionar até o instalares"
  fi
}

# --------------------------------------------------------------------- binário
if [ "$WITH_BINARY" = 1 ]; then
  if [ "$USER_INSTALL" = 1 ]; then
    BIN_DIR="$REAL_HOME/.local/bin"
    mkdir -p "$BIN_DIR"
    BIN_SUDO=""
  else
    BIN_DIR="/usr/local/bin"
    BIN_SUDO="$SUDO"
  fi
  if [ "$VERSION" = latest ]; then
    BASE_URL="https://github.com/$REPO/releases/latest/download"
  else
    BASE_URL="https://github.com/$REPO/releases/download/$VERSION"
  fi
  command -v curl >/dev/null 2>&1 || require_dep deps curl curl "download de releases"
  TMP=$(mktemp -d)
  trap 'rm -rf "$TMP"' EXIT
  # Variante optimizada para o CPU (x86-64-v3: AVX2/BMI2/FMA) quando ele a
  # suporta; releases antigas podem não a ter — fallback para o genérico.
  fetch_asset() { # $1 nome-base (delonix|delonix-cri) → devolve o nome descarregado
    local base="$1" asset="$1-x86_64${CPU_VARIANT}-linux"
    if [ -n "$CPU_VARIANT" ] && ! curl -fsSL -o "$TMP/$asset" "$BASE_URL/$asset" 2>/dev/null; then
      warn "$asset não existe nesta release — a usar o binário genérico"
      asset="$base-x86_64-linux"
      curl -fsSL -o "$TMP/$asset" "$BASE_URL/$asset"
    elif [ -z "$CPU_VARIANT" ]; then
      curl -fsSL -o "$TMP/$asset" "$BASE_URL/$asset"
    fi
    echo "$asset"
  }
  verify_asset() { # nunca instalar um download sem conferir contra o SHA256SUMS
    ( cd "$TMP" && grep -E " $1\$" SHA256SUMS | sha256sum -c - >/dev/null 2>&1 ) \
      || die "verificação SHA256 FALHOU para $1 — download corrompido ou adulterado, a abortar"
  }
  step binario delonix "a descarregar ($VERSION, $VARIANT_LABEL)..."
  curl -fsSL -o "$TMP/SHA256SUMS" "$BASE_URL/SHA256SUMS"
  DELONIX_ASSET=$(fetch_asset delonix)
  verify_asset "$DELONIX_ASSET"
  step binario delonix "sha256 verificado ($DELONIX_ASSET)"
  $BIN_SUDO install -m 0755 "$TMP/$DELONIX_ASSET" "$BIN_DIR/delonix"
  stepok binario "delonix -> $BIN_DIR/delonix"
  if [ "$WITH_CRI" = 1 ]; then
    CRI_ASSET=$(fetch_asset delonix-cri)
    verify_asset "$CRI_ASSET"
    $BIN_SUDO install -m 0755 "$TMP/$CRI_ASSET" "$BIN_DIR/delonix-cri"
    stepok binario "delonix-cri -> $BIN_DIR/delonix-cri"
  fi
  case ":$PATH:" in *":$BIN_DIR:"*) ;; *) warn "$BIN_DIR não está no teu PATH" ;; esac
  # Um delonix ANTIGO mais à frente no PATH faz sombra ao acabado de instalar
  # (caso real: um build 0.3.0 em ~/.local/bin escondia o 0.4.2 e ressuscitava
  # bugs já corrigidos). Detectar e dizer alto qual apagar.
  ACTIVE=$(command -v delonix 2>/dev/null || true)
  if [ -n "$ACTIVE" ] && [ "$ACTIVE" != "$BIN_DIR/delonix" ]; then
    warn "outro delonix faz sombra ao instalado: '$ACTIVE' ($("$ACTIVE" --version 2>/dev/null || echo versão desconhecida)) vem primeiro no PATH — remove-o (rm $ACTIVE) para usares o $BIN_DIR/delonix"
  fi
else
  BIN_DIR=$(dirname "$(command -v delonix 2>/dev/null || echo /usr/local/bin/delonix)")
fi

# ------------------------------------------------- dependências core (containers)
require_dep deps slirp4netns slirp4netns                    "rede rootless / portas publicadas"
require_dep deps newuidmap   "uidmap|shadow-utils|shadow"   "containers rootless multi-uid (imagens com utilizador não-root)"
require_dep deps nft         nftables                       "firewall/DNAT da SDN"
require_dep deps ip          "iproute2|iproute"             "interfaces da SDN (veth/bridge)"
optional_dep deps conntrack  "conntrack|conntrack-tools"    "limpeza de ligações ao despublicar portas"

# ------------------------------------------------------------- subuid / subgid
# Sem um intervalo de subuids, o userns rootless só mapeia 1 uid — qualquer
# imagem com USER não-root falha. É EXACTAMENTE o erro que motivou este script.
ensure_subid() {
  local file="$1" flag="$2"
  if grep -q "^$REAL_USER:" "$file" 2>/dev/null; then
    skip rootless "${file#/etc/}"
    return 0
  fi
  step rootless "${file#/etc/}" "a adicionar intervalo 100000-165535 para $REAL_USER..."
  if command -v usermod >/dev/null 2>&1 && $SUDO usermod "$flag" 100000-165535 "$REAL_USER" 2>/dev/null; then
    stepok rootless "${file#/etc/}"
  else
    # Distros com usermod sem suporte a --add-subuids: append directo.
    echo "$REAL_USER:100000:65536" | $SUDO tee -a "$file" >/dev/null
    stepok rootless "${file#/etc/}"
  fi
}
ensure_subid /etc/subuid --add-subuids
ensure_subid /etc/subgid --add-subgids

# ------------------------------------------- AppArmor (Ubuntu 23.10+/derivados)
# Com kernel.apparmor_restrict_unprivileged_userns=1, um binário sem perfil não
# pode criar user namespaces — o delonix morreria logo no unshare(). O perfil
# unconfined+userns é o mecanismo OFICIAL do Ubuntu para autorizar uma app.
if [ "$(sysctl -n kernel.apparmor_restrict_unprivileged_userns 2>/dev/null || echo 0)" = 1 ]; then
  step rootless apparmor "o host restringe userns não-privilegiados — a instalar perfil..."
  if command -v apparmor_parser >/dev/null 2>&1; then
    printf 'abi <abi/4.0>,\ninclude <tunables/global>\nprofile delonix %s/delonix flags=(unconfined) {\n  userns,\n}\n' "$BIN_DIR" \
      | $SUDO tee /etc/apparmor.d/delonix >/dev/null
    $SUDO apparmor_parser -r /etc/apparmor.d/delonix \
      && stepok rootless apparmor \
      || warn "não consegui carregar o perfil AppArmor — containers rootless podem falhar no arranque"
  else
    warn "apparmor_parser ausente com a restrição de userns activa — instala o apparmor ou põe kernel.apparmor_restrict_unprivileged_userns=0"
  fi
fi
# Debian antigo: userns desligado por sysctl dedicado.
if [ "$(sysctl -n kernel.unprivileged_userns_clone 2>/dev/null || echo 1)" = 0 ]; then
  step rootless userns "a activar kernel.unprivileged_userns_clone..."
  echo 'kernel.unprivileged_userns_clone=1' | $SUDO tee /etc/sysctl.d/99-delonix-userns.conf >/dev/null
  $SUDO sysctl -q kernel.unprivileged_userns_clone=1
  stepok rootless userns
fi

# ------------------------------------------------------------ dependências de VM
NEED_RELOGIN=0
if [ "$WITH_VM" = 1 ]; then
  optional_dep vm qemu-img     "qemu-utils|qemu-img|qemu-tools"                    "discos overlay de VM"
  optional_dep vm cloud-localds "cloud-image-utils|cloud-utils"                     "seed ISOs de cloud-init (vm create --ssh-key/--user-data)"
  # Backend preferido onde a distro o empacota (Fedora/Arch/openSUSE); nas
  # famílias Debian não existe no arquivo — o libvirt abaixo é o fallback
  # que o delonix auto-detecta.
  if ! command -v cloud-hypervisor >/dev/null 2>&1; then
    pkg_install cloud-hypervisor >/dev/null 2>&1 \
      && stepok vm cloud-hypervisor \
      || step vm cloud-hypervisor "não empacotado nesta distro — o backend será o libvirt"
  else
    skip vm cloud-hypervisor
  fi
  optional_dep vm virsh "libvirt-clients|libvirt-client|libvirt"                    "backend de VM libvirt"
  if ! command -v qemu-system-x86_64 >/dev/null 2>&1 && [ ! -e /usr/libexec/qemu-kvm ]; then
    step vm qemu-kvm "a instalar..."
    pkg_install "qemu-system-x86|qemu-kvm|qemu-base|qemu" >/dev/null 2>&1 \
      && stepok vm qemu-kvm || warn "não consegui instalar o QEMU — VMs libvirt não vão arrancar"
  else
    skip vm qemu-kvm
  fi
  pkg_install "libvirt-daemon-system|libvirt-daemon-qemu|libvirt-daemon-kvm|libvirt" >/dev/null 2>&1 || true
  # libvirtd activo (socket-activated onde suportado).
  if command -v systemctl >/dev/null 2>&1; then
    $SUDO systemctl enable --now libvirtd.socket >/dev/null 2>&1 \
      || $SUDO systemctl enable --now libvirtd >/dev/null 2>&1 \
      || warn "não consegui activar o libvirtd — arranca-o manualmente antes de criar VMs"
  fi
  # Acesso a /dev/kvm e ao socket do libvirt sem sudo.
  for grp in kvm libvirt; do
    if getent group "$grp" >/dev/null 2>&1 && ! id -nG "$REAL_USER" | tr ' ' '\n' | grep -qx "$grp"; then
      $SUDO usermod -aG "$grp" "$REAL_USER" && { stepok vm "grupo $grp ($REAL_USER adicionado)"; NEED_RELOGIN=1; }
    fi
  done
  if [ ! -e /dev/kvm ]; then
    warn "/dev/kvm não existe — virtualização por hardware desligada (activa VT-x/AMD-V na BIOS) ou estás numa VM sem nested virt"
  fi
fi

# ------------------------------------------------- tuning de kernel (opt-out)
# Sysctls/módulos que containers, Kubernetes e VMs exigem ou esgotam depressa.
# Cada linha tem uma razão concreta — nada de "tuning" de folclore:
#   inotify         — kubelet/hot-reload esgotam os defaults com poucas dezenas
#                     de containers ("too many open files" enganador).
#   ip_forward      — NAT do libvirt e CNI de k8s precisam de routing no host.
#   overlay         — overlayfs das imagens (carregado on-demand, mas em boot
#                     lockdown/containers aninhados o autoload falha).
#   br_netfilter + bridge-nf-call — requisito documentado do kubeadm (o
#                     kube-proxy precisa de ver tráfego bridged no netfilter).
#   tun             — slirp4netns/VMs precisam de /dev/net/tun desde o boot.
#   max_map_count   — bases de dados/JVMs em containers (Elasticsearch exige-o).
#   ping_group_range — ping dentro de containers rootless sem CAP_NET_RAW.
if [ "$WITH_TUNE" = 1 ]; then
  step kernel modulos "a carregar overlay/br_netfilter/tun..."
  printf '%s\n' overlay br_netfilter tun | $SUDO tee /etc/modules-load.d/delonix.conf >/dev/null
  for m in overlay br_netfilter tun; do $SUDO modprobe "$m" 2>/dev/null || true; done
  stepok kernel modulos
  step kernel sysctls "a aplicar (inotify/ip_forward/bridge-nf/max_map_count)..."
  $SUDO tee /etc/sysctl.d/99-delonix.conf >/dev/null <<'SYSCTL'
# Delonix Runtime — tuning para containers/k8s/VMs (gerado pelo install.sh).
fs.inotify.max_user_watches = 1048576
fs.inotify.max_user_instances = 8192
net.ipv4.ip_forward = 1
net.bridge.bridge-nf-call-iptables = 1
net.bridge.bridge-nf-call-ip6tables = 1
vm.max_map_count = 262144
net.core.somaxconn = 4096
net.ipv4.ping_group_range = 0 2147483647
SYSCTL
  if $SUDO sysctl -q -p /etc/sysctl.d/99-delonix.conf >/dev/null 2>&1; then
    stepok kernel sysctls
  else
    warn "alguns sysctls não aplicaram (kernel sem o módulo?) — voltam a tentar no próximo boot"
  fi
fi

# ------------------------------------------------------------ completion (bash)
if [ "$WITH_BINARY" = 1 ] && [ -d /etc/bash_completion.d ] && [ -x "$BIN_DIR/delonix" ]; then
  "$BIN_DIR/delonix" completion bash 2>/dev/null | $SUDO tee /etc/bash_completion.d/delonix >/dev/null \
    && stepok binario "bash completion" || true
fi

# ----------------------------------------------------------------- verificação
msg "a verificar a instalação..."
FAIL=0
check() { # $1 descrição, $2.. comando
  local desc="$1"; shift
  if "$@" >/dev/null 2>&1; then stepok verificar "$desc"; else
    printf '[verificar] %s: %sFALHOU%s\n' "$desc" "$C_ERR" "$C_0"; FAIL=1
  fi
}
[ "$WITH_BINARY" = 1 ] && check "delonix ($("$BIN_DIR/delonix" --version 2>/dev/null || echo '?'))" "$BIN_DIR/delonix" --version
check "slirp4netns"                    has_cmd slirp4netns
check "newuidmap"                      has_cmd newuidmap
check "newuidmap privilegiado"         sh -c 'nm=$(command -v newuidmap) && { [ -u "$nm" ] || getcap "$nm" 2>/dev/null | grep -q cap_setuid; }'
check "nft"                            has_cmd nft
check "subuid de $REAL_USER"           grep -q "^$REAL_USER:" /etc/subuid
check "subgid de $REAL_USER"           grep -q "^$REAL_USER:" /etc/subgid
check "/dev/net/tun"                   test -e /dev/net/tun
check "user namespaces"                unshare -r -n true
if [ "$WITH_VM" = 1 ]; then
  check "backend de VM (cloud-hypervisor ou virsh)" sh -c 'command -v cloud-hypervisor || command -v virsh'
fi

echo
if [ "$FAIL" = 0 ]; then
  msg "pronto"
  echo "    delonix container run -d -p 8080:80 nginx && curl localhost:8080"
else
  warn "instalação terminou com avisos — revê as linhas FALHOU acima"
fi
if [ "$NEED_RELOGIN" = 1 ]; then
  warn "termina a sessão e volta a entrar (ou 'newgrp kvm') para os novos grupos terem efeito"
fi
