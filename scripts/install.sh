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
WITH_BINARY=1
WITH_CRI=0
USER_INSTALL=0

# `command -v` falha para binários de admin (/usr/sbin) quando o PATH do
# utilizador não os inclui (Debian) — mas o delonix invoca-os pelo PATH do
# processo, e o dos serviços/sudo inclui sbin. Procurar lá também.
has_cmd() { command -v "$1" >/dev/null 2>&1 || [ -x "/usr/sbin/$1" ] || [ -x "/sbin/$1" ]; }

msg()  { printf '\033[1;36m==>\033[0m %s\n' "$*"; }
ok()   { printf '\033[1;32m ✓ \033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m ! \033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31m ✗ \033[0m %s\n' "$*" >&2; exit 1; }

while [ $# -gt 0 ]; do
  case "$1" in
    --no-vm)      WITH_VM=0 ;;
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
  msg "Some steps need root — sudo may prompt for your password."
  # Autentica JÁ: assim, um falhanço de pkg_install mais à frente significa
  # mesmo "pacote indisponível", nunca "sudo falhou em silêncio".
  sudo -v || die "sudo authentication failed — run again and enter your password, or run as root"
fi

# ------------------------------------------------------------ detecção de distro
PKG=""
if [ -r /etc/os-release ]; then
  . /etc/os-release
  DISTRO_IDS="${ID:-} ${ID_LIKE:-}"
else
  DISTRO_IDS=""
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
msg "Distro family: $PKG (${PRETTY_NAME:-unknown})"

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
  # $1 = comando que tem de existir no fim; $2 = candidatos de pacote; $3 = razão
  local cmd="$1" pkgs="$2" why="$3"
  if has_cmd "$cmd"; then ok "$cmd already installed"; return 0; fi
  msg "Installing $cmd ($why)..."
  pkg_install "$pkgs" || die "could not install '$pkgs' — install it with your package manager and re-run"
  has_cmd "$cmd" || die "'$pkgs' installed but '$cmd' still not found"
  ok "$cmd installed"
}

optional_dep() {
  local cmd="$1" pkgs="$2" why="$3"
  if has_cmd "$cmd"; then ok "$cmd already installed"; return 0; fi
  msg "Installing $cmd ($why)..."
  if pkg_install "$pkgs" && has_cmd "$cmd"; then
    ok "$cmd installed"
  else
    warn "$cmd not available on this distro — $why will not work until you install it"
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
  command -v curl >/dev/null 2>&1 || require_dep curl curl "downloading releases"
  TMP=$(mktemp -d)
  trap 'rm -rf "$TMP"' EXIT
  msg "Downloading delonix ($VERSION) ..."
  curl -fsSL -o "$TMP/delonix-x86_64-linux" "$BASE_URL/delonix-x86_64-linux"
  curl -fsSL -o "$TMP/SHA256SUMS" "$BASE_URL/SHA256SUMS"
  if [ "$WITH_CRI" = 1 ]; then
    curl -fsSL -o "$TMP/delonix-cri-x86_64-linux" "$BASE_URL/delonix-cri-x86_64-linux"
  fi
  # Verificação de integridade — nunca instalar um download sem conferir.
  ( cd "$TMP"
    grep -E ' (delonix|delonix-cri)-x86_64-linux$' SHA256SUMS \
      | { [ "$WITH_CRI" = 1 ] && cat || grep -v cri; } \
      | sha256sum -c - >/dev/null ) || die "SHA256 verification FAILED — corrupted or tampered download, aborting"
  ok "SHA256 verified"
  $BIN_SUDO install -m 0755 "$TMP/delonix-x86_64-linux" "$BIN_DIR/delonix"
  ok "delonix -> $BIN_DIR/delonix"
  if [ "$WITH_CRI" = 1 ]; then
    $BIN_SUDO install -m 0755 "$TMP/delonix-cri-x86_64-linux" "$BIN_DIR/delonix-cri"
    ok "delonix-cri -> $BIN_DIR/delonix-cri"
  fi
  case ":$PATH:" in *":$BIN_DIR:"*) ;; *) warn "$BIN_DIR is not in your PATH" ;; esac
else
  BIN_DIR=$(dirname "$(command -v delonix 2>/dev/null || echo /usr/local/bin/delonix)")
fi

# ------------------------------------------------- dependências core (containers)
msg "Installing container runtime dependencies..."
require_dep slirp4netns slirp4netns                    "rootless networking / published ports"
require_dep newuidmap   "uidmap|shadow-utils|shadow"   "multi-uid rootless containers (images with non-root users)"
require_dep nft         nftables                       "SDN firewall / port DNAT"
require_dep ip          "iproute2|iproute"             "SDN interfaces (veth/bridge)"
optional_dep conntrack  "conntrack|conntrack-tools"    "connection cleanup on port unpublish"

# ------------------------------------------------------------- subuid / subgid
# Sem um intervalo de subuids, o userns rootless só mapeia 1 uid — qualquer
# imagem com USER não-root falha. É EXACTAMENTE o erro que motivou este script.
ensure_subid() {
  local file="$1" flag="$2"
  if grep -q "^$REAL_USER:" "$file" 2>/dev/null; then
    ok "$file already has an entry for $REAL_USER"
    return 0
  fi
  msg "Adding $REAL_USER to $file (range 100000-165535)..."
  if command -v usermod >/dev/null 2>&1 && $SUDO usermod "$flag" 100000-165535 "$REAL_USER" 2>/dev/null; then
    ok "$file configured via usermod"
  else
    # Distros com usermod sem suporte a --add-subuids: append directo.
    echo "$REAL_USER:100000:65536" | $SUDO tee -a "$file" >/dev/null
    ok "$file configured"
  fi
}
ensure_subid /etc/subuid --add-subuids
ensure_subid /etc/subgid --add-subgids

# ------------------------------------------- AppArmor (Ubuntu 23.10+/derivados)
# Com kernel.apparmor_restrict_unprivileged_userns=1, um binário sem perfil não
# pode criar user namespaces — o delonix morreria logo no unshare(). O perfil
# unconfined+userns é o mecanismo OFICIAL do Ubuntu para autorizar uma app.
if [ "$(sysctl -n kernel.apparmor_restrict_unprivileged_userns 2>/dev/null || echo 0)" = 1 ]; then
  msg "AppArmor restricts unprivileged user namespaces on this host — installing profile..."
  if command -v apparmor_parser >/dev/null 2>&1; then
    printf 'abi <abi/4.0>,\ninclude <tunables/global>\nprofile delonix %s/delonix flags=(unconfined) {\n  userns,\n}\n' "$BIN_DIR" \
      | $SUDO tee /etc/apparmor.d/delonix >/dev/null
    $SUDO apparmor_parser -r /etc/apparmor.d/delonix \
      && ok "AppArmor profile loaded (/etc/apparmor.d/delonix)" \
      || warn "could not load the AppArmor profile — rootless containers may fail to start"
  else
    warn "apparmor_parser not found but the userns restriction is active — install apparmor or set kernel.apparmor_restrict_unprivileged_userns=0"
  fi
fi
# Debian antigo: userns desligado por sysctl dedicado.
if [ "$(sysctl -n kernel.unprivileged_userns_clone 2>/dev/null || echo 1)" = 0 ]; then
  msg "Enabling unprivileged user namespaces (kernel.unprivileged_userns_clone)..."
  echo 'kernel.unprivileged_userns_clone=1' | $SUDO tee /etc/sysctl.d/99-delonix-userns.conf >/dev/null
  $SUDO sysctl -q kernel.unprivileged_userns_clone=1
  ok "user namespaces enabled (persisted in /etc/sysctl.d/99-delonix-userns.conf)"
fi

# ------------------------------------------------------------ dependências de VM
NEED_RELOGIN=0
if [ "$WITH_VM" = 1 ]; then
  msg "Installing microVM dependencies..."
  optional_dep qemu-img     "qemu-utils|qemu-img|qemu-tools"                        "VM overlay disks"
  optional_dep cloud-localds "cloud-image-utils|cloud-utils"                        "cloud-init seed ISOs (vm create --ssh-key/--user-data)"
  # Backend preferido onde a distro o empacota (Fedora/Arch/openSUSE); nas
  # famílias Debian não existe no arquivo — o libvirt abaixo é o fallback
  # que o delonix auto-detecta.
  if ! command -v cloud-hypervisor >/dev/null 2>&1; then
    pkg_install cloud-hypervisor >/dev/null 2>&1 \
      && ok "cloud-hypervisor installed (preferred VM backend)" \
      || msg "cloud-hypervisor not packaged on this distro — using libvirt backend"
  else
    ok "cloud-hypervisor already installed"
  fi
  optional_dep virsh "libvirt-clients|libvirt-client|libvirt"                       "libvirt VM backend"
  if ! command -v qemu-system-x86_64 >/dev/null 2>&1 && [ ! -e /usr/libexec/qemu-kvm ]; then
    pkg_install "qemu-system-x86|qemu-kvm|qemu-base|qemu" >/dev/null 2>&1 \
      && ok "QEMU/KVM installed" || warn "could not install QEMU — libvirt VMs will not start"
  else
    ok "QEMU/KVM already installed"
  fi
  pkg_install "libvirt-daemon-system|libvirt-daemon-qemu|libvirt-daemon-kvm|libvirt" >/dev/null 2>&1 || true
  # libvirtd activo (socket-activated onde suportado).
  if command -v systemctl >/dev/null 2>&1; then
    $SUDO systemctl enable --now libvirtd.socket >/dev/null 2>&1 \
      || $SUDO systemctl enable --now libvirtd >/dev/null 2>&1 \
      || warn "could not enable libvirtd — start it manually before creating VMs"
  fi
  # Acesso a /dev/kvm e ao socket do libvirt sem sudo.
  for grp in kvm libvirt; do
    if getent group "$grp" >/dev/null 2>&1 && ! id -nG "$REAL_USER" | tr ' ' '\n' | grep -qx "$grp"; then
      $SUDO usermod -aG "$grp" "$REAL_USER" && { ok "added $REAL_USER to group '$grp'"; NEED_RELOGIN=1; }
    fi
  done
  if [ ! -e /dev/kvm ]; then
    warn "/dev/kvm does not exist — hardware virtualization is disabled (enable VT-x/AMD-V in the BIOS) or you are inside a VM without nested virt"
  fi
fi

# ------------------------------------------------------------ completion (bash)
if [ "$WITH_BINARY" = 1 ] && [ -d /etc/bash_completion.d ] && [ -x "$BIN_DIR/delonix" ]; then
  "$BIN_DIR/delonix" completion bash 2>/dev/null | $SUDO tee /etc/bash_completion.d/delonix >/dev/null \
    && ok "bash completion installed" || true
fi

# ----------------------------------------------------------------- verificação
msg "Verifying the installation..."
FAIL=0
check() { # $1 descrição, $2.. comando
  local desc="$1"; shift
  if "$@" >/dev/null 2>&1; then ok "$desc"; else warn "$desc — FAILED"; FAIL=1; fi
}
[ "$WITH_BINARY" = 1 ] && check "delonix runs ($("$BIN_DIR/delonix" --version 2>/dev/null || echo '?'))" "$BIN_DIR/delonix" --version
check "slirp4netns present"            command -v slirp4netns
check "newuidmap present"              command -v newuidmap
check "newuidmap is privileged (setuid/caps)" sh -c 'nm=$(command -v newuidmap) && { [ -u "$nm" ] || getcap "$nm" 2>/dev/null | grep -q cap_setuid; }'
check "nft present"                    command -v nft
check "subuid range for $REAL_USER"    grep -q "^$REAL_USER:" /etc/subuid
check "subgid range for $REAL_USER"    grep -q "^$REAL_USER:" /etc/subgid
check "/dev/net/tun available"         test -e /dev/net/tun
check "user namespaces usable"         unshare -r -n true
if [ "$WITH_VM" = 1 ]; then
  check "a VM backend (cloud-hypervisor or virsh)" sh -c 'command -v cloud-hypervisor || command -v virsh'
fi

echo
if [ "$FAIL" = 0 ]; then
  ok "Delonix Runtime is ready. Try it:"
  echo "      delonix container run -d -p 8080:80 nginx && curl localhost:8080"
else
  warn "installation finished with warnings — review the FAILED lines above"
fi
if [ "$NEED_RELOGIN" = 1 ]; then
  warn "log out and back in (or run 'newgrp kvm') for the new group memberships to take effect"
fi
