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
#   --low-ports    permite publicar portas <1024 (ex.: 80/443) sem root.
#                  NÃO é o default — ver a secção "portas privilegiadas" abaixo.
#   --insecure-skip-signature
#                  NÃO verificar a assinatura da release. Só para depurar ou
#                  para uma release antiga sem assinatura. Perde a única
#                  garantia de que o binário veio mesmo de nós.
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

# ---------------------------------------------------------------------------
# TUDO o que se segue está dentro de `{ ... }` — a chaveta final está no fim do
# ficheiro. NÃO é decoração: o uso documentado é `curl … | bash`, e o bash
# EXECUTA À MEDIDA QUE LÊ. Sem esta chaveta, uma transferência cortada a meio
# (rede a cair, proxy a truncar) executa METADE do instalador — e esta metade
# instala pacotes com sudo, acrescenta a /etc/subuid, escreve perfis AppArmor e
# ficheiros em /etc/sysctl.d. Um host meio-configurado, sem um único erro.
# Com a chaveta, o bash tem de ler até ao `}` antes de executar seja o que for:
# um ficheiro truncado morre em "syntax error: unexpected end of file" e NADA
# corre. Verificado empiricamente, não assumido.
# ---------------------------------------------------------------------------
{

REPO="angolardevops/delonix-runtime"
# Chave PÚBLICA minisign das releases oficiais (só a linha base64, sem o
# comentário). É a raiz de confiança do instalador: o SHA256SUMS é assinado em
# CI com a privada correspondente, que nunca sai do secret do repositório.
#
# PORQUE ISTO EXISTE: verificar o binário contra um SHA256SUMS descarregado da
# MESMA URL prova só que a transferência não corrompeu — não prova quem o
# produziu. Quem consiga publicar uma release adulterada publica também o
# SHA256SUMS a condizer, e a verificação passa. Com a assinatura, forjar exige
# a chave privada.
#
# LIMITE HONESTO: isto protege os BINÁRIOS. O próprio install.sh, quando corrido
# por `curl … | bash`, é executado sem verificação — a sua autenticidade depende
# do TLS e do GitHub. Para fechar também esse elo, descarrega-o primeiro e
# confere-o contra o SHA256SUMS assinado antes de o correr (ver README).
MINISIGN_PUBKEY="RWSiOqlKAnVVB+pJLQxgYHq/kdN6RbBQdlL5gOcZ6H/xkwSAPIqTo+GB"
VERSION="latest"
WITH_VM=1
WITH_TUNE=1
WITH_BINARY=1
WITH_CRI=0
USER_INSTALL=0
LOW_PORTS=0
SKIP_SIG=0

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
# Limpeza única no EXIT: TMP (se criado) + repor o cursor que o spin esconde.
CLEANUP_TMP=""
cleanup() {
  [ -n "$CLEANUP_TMP" ] && rm -rf "$CLEANUP_TMP"
  [ -t 1 ] && printf '\033[?25h'
  return 0
}
trap cleanup EXIT

msg()   { printf 'install/delonix: %s\n' "$*"; }
step()  { printf '[%s] %s: %s\n' "$1" "$2" "$3"; }                    # estado neutro
skip()  { printf '[%s] %s: %salready satisfied (SKIP)%s\n' "$1" "$2" "$C_SKIP" "$C_0"; }
stepok(){ printf '[%s] %s: %sOK%s\n' "$1" "$2" "$C_OK" "$C_0"; }
warn()  { printf '%swarning%s %s\n' "$C_WARN" "$C_0" "$*" >&2; }
die()   { printf '%serror%s %s\n' "$C_ERR" "$C_0" "$*" >&2; exit 1; }

# Corre um comando com um SPINNER animado na linha do passo (só em tty; em
# pipe/CI degrada para a linha estática de sempre). O comando corre em
# background no MESMO shell (funções e variáveis visíveis); devolve o rc dele.
#   spin <fase> <nome> <texto-em-curso> <cmd...>
SPIN_FRAMES=('⠋' '⠙' '⠹' '⠸' '⠼' '⠴' '⠦' '⠧' '⠇' '⠏')
spin() {
  local phase="$1" name="$2" doing="$3"; shift 3
  if [ ! -t 1 ]; then
    step "$phase" "$name" "$doing"
    "$@"
    return $?
  fi
  "$@" &
  local pid=$! i=0
  printf '\033[?25l'
  while kill -0 "$pid" 2>/dev/null; do
    printf '\r\033[K[%s] %s: %s %s' "$phase" "$name" "$doing" "${SPIN_FRAMES[i % 10]}"
    i=$((i + 1))
    sleep 0.1
  done
  local rc=0
  wait "$pid" || rc=$?
  printf '\r\033[K\033[?25h'
  return $rc
}

while [ $# -gt 0 ]; do
  case "$1" in
    --no-vm)      WITH_VM=0 ;;
    --no-tune)    WITH_TUNE=0 ;;
    --no-binary)  WITH_BINARY=0 ;;
    --with-cri)   WITH_CRI=1 ;;
    --low-ports)  LOW_PORTS=1 ;;
    --insecure-skip-signature) SKIP_SIG=1 ;;
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
  msg "some steps need root — sudo may ask for your password"
  # Autentica JÁ: assim, um falhanço de pkg_install mais à frente significa
  # mesmo "pacote indisponível", nunca "sudo falhou em silêncio".
  sudo -v || die "sudo authentication failed — run again and enter your password, or run as root"
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
msg "preparing the host ($DISTRO_NAME, $PKG package manager)..."

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
  # `|| true` is NOT cosmetic. Under `set -euo pipefail`, a `grep` that matches
  # NOTHING exits 1, `pipefail` propagates it, and the assignment fails — which
  # aborted the whole installer, silently, right after the "preparing the host"
  # line. Any machine with no VGA/3D device hits it: every headless server and
  # essentially every VM. Reproduced in a clean Ubuntu 24.04 VM, where the
  # installer died before installing anything and printed no error at all.
  # A cosmetic GPU label must never be able to fail an installation.
  GPU_INFO=$(lspci 2>/dev/null | grep -Ei 'vga|3d controller' \
    | sed -E 's/^[0-9a-f:.]+ +//; s/^(VGA compatible controller|3D controller): +//' \
    | paste -sd ';' - | sed 's/;/ · /g' || true)
elif [ -d /sys/class/drm ] && ls /sys/class/drm/card[0-9] >/dev/null 2>&1; then
  GPU_INFO="present (install pciutils for details)"
fi
CPU_VARIANT=""
# x86-64-v3 = AVX2+BMI2+FMA. O teu binário genérico continua a ser o fallback.
if grep -qm1 avx2 /proc/cpuinfo && grep -qm1 bmi2 /proc/cpuinfo && grep -qm1 fma /proc/cpuinfo; then
  CPU_VARIANT="-v3"
fi
if [ -n "$CPU_VARIANT" ]; then VARIANT_LABEL="x86-64-v3 (AVX2)"; else VARIANT_LABEL="x86-64 baseline"; fi
step host cpu "${CPU_MODEL:-unknown} (${NCPU} cpus, $VARIANT_LABEL)"
step host resources "${RAM_GB}GB RAM · ${DISK_FREE_GB:-?}GB free at $REAL_HOME"
[ -n "$GPU_INFO" ] && step host gpu "$GPU_INFO"
[ "${RAM_GB:-8}" != '?' ] && [ "${RAM_GB:-8}" -lt 2 ] 2>/dev/null && warn "less than 2GB of RAM — VMs will be tight; containers are fine"
[ -n "$DISK_FREE_GB" ] && [ "$DISK_FREE_GB" -lt 10 ] 2>/dev/null && warn "less than 10GB free — image pulls and container rootfs fill the disk fast (the kubelet evicts pods under disk pressure)"

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

# O índice de pacotes actualiza-se UMA vez, no shell principal — o spin corre
# o pkg_install em subshell e a mutação de PKG_UPDATED perder-se-ia lá dentro.
pkg_update_once() {
  [ "$PKG_UPDATED" = 1 ] && return 0
  case "$PKG" in
    apt) $SUDO apt-get update -qq >/dev/null 2>&1 || true ;;
    pacman) $SUDO pacman -Sy --noconfirm >/dev/null 2>&1 || true ;;
  esac
  PKG_UPDATED=1
}

require_dep() {
  # $1 = fase; $2 = comando que tem de existir no fim; $3 = candidatos; $4 = razão
  local phase="$1" cmd="$2" pkgs="$3" why="$4"
  if has_cmd "$cmd"; then skip "$phase" "$cmd"; return 0; fi
  pkg_update_once
  spin "$phase" "$cmd" "installing ($why)..." pkg_install "$pkgs" \
    || die "could not install '$pkgs' — install it with your package manager and re-run"
  has_cmd "$cmd" || die "'$pkgs' installed but '$cmd' is still missing"
  stepok "$phase" "$cmd"
}

optional_dep() {
  local phase="$1" cmd="$2" pkgs="$3" why="$4"
  if has_cmd "$cmd"; then skip "$phase" "$cmd"; return 0; fi
  pkg_update_once
  if spin "$phase" "$cmd" "installing ($why)..." pkg_install "$pkgs" && has_cmd "$cmd"; then
    stepok "$phase" "$cmd"
  else
    warn "$cmd unavailable on this distro — $why will not work until you install it"
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
  command -v curl >/dev/null 2>&1 || require_dep deps curl curl "release downloads"
  TMP=$(mktemp -d)
  CLEANUP_TMP="$TMP"
  # Variante optimizada para o CPU (x86-64-v3: AVX2/BMI2/FMA) quando ele a
  # suporta; releases antigas podem não a ter — fallback para o genérico.
  #
  # BUG CORRIGIDO AQUI (achado ao vivo, num host real): esta função e
  # `dl_main` abaixo corriam sob `set -e`, mas terminavam SEMPRE com `echo`
  # (aqui) ou com o `curl` da SHA256SUMS a não ser verificado explicitamente
  # (lá) — e como as duas correm dentro de `spin ... || die`, o `errexit`
  # fica SUSPENSO para toda a árvore de chamadas aninhada sob esse `||`
  # (comportamento documentado do bash: uma falha só dispara o `set -e` se
  # NÃO estiver a ser testada por `&&`/`||`/`if` — e essa suspensão propaga-se
  # para dentro de funções chamadas nesse contexto). Resultado real, visto num
  # host: o `curl` da SHA256SUMS falhava com "Failure when receiving data
  # from the peer" (erro de rede transitório), a falha era engolida em
  # silêncio, e só aparecia depois como "SHA256 verification FAILED —
  # corrupted or tampered download" — mensagem enganosa (implica adulteração/
  # MITM) para o que era só uma transferência que falhou. Corrigido com
  # `|| return 1` explícito em cada `curl` que tem de ser fatal — controlo de
  # fluxo explícito não depende do estado (in)consistente do `errexit`.
  fetch_asset() { # $1 nome-base (delonix|delonix-cri) → devolve o nome descarregado, ou falha
    local base="$1" asset="$1-x86_64${CPU_VARIANT}-linux"
    if [ -n "$CPU_VARIANT" ]; then
      if curl -fsSL -o "$TMP/$asset" "$BASE_URL/$asset" 2>/dev/null; then
        echo "$asset"
        return 0
      fi
      warn "$asset is not in this release — falling back to the generic binary"
      asset="$base-x86_64-linux"
    fi
    curl -fsSL -o "$TMP/$asset" "$BASE_URL/$asset" || return 1
    echo "$asset"
  }
  verify_asset() { # nunca instalar um download sem conferir contra o SHA256SUMS
    [ -s "$TMP/SHA256SUMS" ] \
      || die "could not download SHA256SUMS — check your network and re-run (this is a download failure, not a corrupted/tampered file)"
    ( cd "$TMP" && grep -E " $1\$" SHA256SUMS | sha256sum -c - >/dev/null 2>&1 ) \
      || die "SHA256 verification FAILED for $1 — corrupted or tampered download, aborting"
  }
  # A assinatura é o que distingue "não corrompeu" de "veio mesmo de nós". Corre
  # ANTES de qualquer verify_asset: sem SHA256SUMS autêntico, os hashes que ele
  # contém não valem nada. FAIL-CLOSED — qualquer coisa que corra mal aborta.
  verify_signature() {
    if [ "$SKIP_SIG" = 1 ]; then
      warn "signature verification SKIPPED (--insecure-skip-signature) — you are trusting whatever the network served"
      return 0
    fi
    if [ "$MINISIGN_PUBKEY" = "__POR_PREENCHER__" ]; then
      warn "this install.sh has no release public key embedded — cannot verify authenticity, only transfer integrity"
      return 0
    fi
    if ! command -v minisign >/dev/null 2>&1; then
      step binary signature "installing minisign to verify the release..."
      pkg_install minisign >/dev/null 2>&1 || true
    fi
    command -v minisign >/dev/null 2>&1 \
      || die "minisign is needed to verify the release signature and could not be installed — install it (apt/dnf/pacman install minisign) and re-run, or use --insecure-skip-signature if you accept the risk"
    curl -fsSL -o "$TMP/SHA256SUMS.minisig" "$BASE_URL/SHA256SUMS.minisig" \
      || die "this release has no signature (SHA256SUMS.minisig) — releases before signing was introduced need --insecure-skip-signature"
    printf 'untrusted comment: delonix-runtime release key\n%s\n' "$MINISIGN_PUBKEY" > "$TMP/delonix.pub"
    ( cd "$TMP" && minisign -V -p delonix.pub -m SHA256SUMS >/dev/null 2>&1 ) \
      || die "SIGNATURE verification FAILED for SHA256SUMS — this release was NOT signed by the delonix key. Do not install. Report it at https://github.com/$REPO/security"
    step binary signature "minisign verified (SHA256SUMS)"
  }
  dl_main() {
    curl -fsSL -o "$TMP/SHA256SUMS" "$BASE_URL/SHA256SUMS" || return 1
    fetch_asset delonix > "$TMP/.asset-delonix"
  }
  spin binary delonix "downloading ($VERSION, $VARIANT_LABEL)..." dl_main \
    || die "download failed — check the network and that the release exists"
  verify_signature
  DELONIX_ASSET=$(cat "$TMP/.asset-delonix")
  verify_asset "$DELONIX_ASSET"
  step binary delonix "sha256 verified ($DELONIX_ASSET)"
  $BIN_SUDO install -m 0755 "$TMP/$DELONIX_ASSET" "$BIN_DIR/delonix"
  stepok binary "delonix -> $BIN_DIR/delonix"
  if [ "$WITH_CRI" = 1 ]; then
    dl_cri() { fetch_asset delonix-cri > "$TMP/.asset-cri"; }
    spin binary delonix-cri "downloading..." dl_cri \
      || die "delonix-cri download failed"
    CRI_ASSET=$(cat "$TMP/.asset-cri")
    verify_asset "$CRI_ASSET"
    $BIN_SUDO install -m 0755 "$TMP/$CRI_ASSET" "$BIN_DIR/delonix-cri"
    stepok binary "delonix-cri -> $BIN_DIR/delonix-cri"
  fi
  case ":$PATH:" in *":$BIN_DIR:"*) ;; *) warn "$BIN_DIR is not in your PATH" ;; esac
  # Um delonix ANTIGO mais à frente no PATH faz sombra ao acabado de instalar
  # (caso real: um build 0.3.0 em ~/.local/bin escondia o 0.4.2 e ressuscitava
  # bugs já corrigidos). Detectar e dizer alto qual apagar.
  ACTIVE=$(command -v delonix 2>/dev/null || true)
  if [ -n "$ACTIVE" ] && [ "$ACTIVE" != "$BIN_DIR/delonix" ]; then
    warn "another delonix shadows the one just installed: '$ACTIVE' ($("$ACTIVE" --version 2>/dev/null || echo unknown version)) comes first in PATH — remove it (rm $ACTIVE) to use $BIN_DIR/delonix"
  fi
else
  BIN_DIR=$(dirname "$(command -v delonix 2>/dev/null || echo /usr/local/bin/delonix)")
fi

# ------------------------------------------------- dependências core (containers)
require_dep deps slirp4netns slirp4netns                    "rootless networking / published ports"
require_dep deps newuidmap   "uidmap|shadow-utils|shadow"   "multi-uid rootless containers (images with non-root users)"
require_dep deps nft         nftables                       "SDN firewall / port DNAT"
require_dep deps ip          "iproute2|iproute"             "SDN interfaces (veth/bridge)"
optional_dep deps conntrack  "conntrack|conntrack-tools"    "connection cleanup on port unpublish"

# ------------------------------------------------------------- subuid / subgid
# Sem um intervalo de subuids, o userns rootless só mapeia 1 uid — qualquer
# imagem com USER não-root falha. É EXACTAMENTE o erro que motivou este script.
ensure_subid() {
  local file="$1" flag="$2"
  if grep -q "^$REAL_USER:" "$file" 2>/dev/null; then
    skip rootless "${file#/etc/}"
    return 0
  fi
  step rootless "${file#/etc/}" "adding range 100000-165535 for $REAL_USER..."
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
  step rootless apparmor "the host restricts unprivileged userns — installing profile..."
  if command -v apparmor_parser >/dev/null 2>&1; then
    printf 'abi <abi/4.0>,\ninclude <tunables/global>\nprofile delonix %s/delonix flags=(unconfined) {\n  userns,\n}\n' "$BIN_DIR" \
      | $SUDO tee /etc/apparmor.d/delonix >/dev/null
    $SUDO apparmor_parser -r /etc/apparmor.d/delonix \
      && stepok rootless apparmor \
      || warn "could not load the AppArmor profile — rootless containers may fail to start"
  else
    warn "apparmor_parser missing while the userns restriction is active — install apparmor or set kernel.apparmor_restrict_unprivileged_userns=0"
  fi
fi
# Debian antigo: userns desligado por sysctl dedicado.
if [ "$(sysctl -n kernel.unprivileged_userns_clone 2>/dev/null || echo 1)" = 0 ]; then
  step rootless userns "enabling kernel.unprivileged_userns_clone..."
  echo 'kernel.unprivileged_userns_clone=1' | $SUDO tee /etc/sysctl.d/99-delonix-userns.conf >/dev/null
  $SUDO sysctl -q kernel.unprivileged_userns_clone=1
  stepok rootless userns
fi

# ------------------------------------------------------------ dependências de VM
NEED_RELOGIN=0
if [ "$WITH_VM" = 1 ]; then
  optional_dep vm qemu-img     "qemu-utils|qemu-img|qemu-tools"                    "VM overlay disks"
  optional_dep vm cloud-localds "cloud-image-utils|cloud-utils"                     "cloud-init seed ISOs (vm create --ssh-key/--user-data)"
  # Backend preferido onde a distro o empacota (Fedora/Arch/openSUSE); nas
  # famílias Debian não existe no arquivo — o libvirt abaixo é o fallback
  # que o delonix auto-detecta.
  # Backend PREFERIDO do motor (delonix-vm tenta-o primeiro; virsh é fallback).
  # Onde a distro não o empacota (famílias Debian/Ubuntu), instala o binário
  # ESTÁTICO oficial do upstream — HTTPS do repo oficial; o upstream não publica
  # checksums, por isso não há verificação de hash (fica anotado).
  if ! command -v cloud-hypervisor >/dev/null 2>&1; then
    if pkg_install cloud-hypervisor >/dev/null 2>&1; then
      stepok vm cloud-hypervisor
    else
      CH_URL="https://github.com/cloud-hypervisor/cloud-hypervisor/releases/latest/download/cloud-hypervisor-static"
      fetch_ch() {
        curl -fsSL -o /tmp/cloud-hypervisor-static.$$ "$CH_URL" \
          && $SUDO install -m 0755 /tmp/cloud-hypervisor-static.$$ /usr/local/bin/cloud-hypervisor
      }
      if spin vm cloud-hypervisor "not packaged on this distro — fetching the official static binary..." fetch_ch; then
        rm -f /tmp/cloud-hypervisor-static.$$
        stepok vm "cloud-hypervisor -> /usr/local/bin/cloud-hypervisor ($(/usr/local/bin/cloud-hypervisor --version 2>/dev/null | head -1))"
      else
        rm -f /tmp/cloud-hypervisor-static.$$
        warn "could not fetch cloud-hypervisor — the libvirt backend below remains the fallback"
      fi
    fi
  else
    skip vm cloud-hypervisor
  fi
  # Firmware do Cloud Hypervisor (rust-hypervisor-fw): o CH não tem BIOS, por
  # isso uma cloud image (a golden `delonix vm pull`) só arranca com firmware.
  # O motor procura-o em /usr/local/share/delonix/hypervisor-fw — sem ele,
  # `vm create` de uma cloud image falharia. Binário minúsculo (~150 KB).
  FW_DEST=/usr/local/share/delonix/hypervisor-fw
  if [ ! -e "$FW_DEST" ]; then
    FW_URL="https://github.com/cloud-hypervisor/rust-hypervisor-firmware/releases/latest/download/hypervisor-fw"
    fetch_fw() {
      $SUDO mkdir -p /usr/local/share/delonix \
        && curl -fsSL -o /tmp/hypervisor-fw.$$ "$FW_URL" \
        && $SUDO install -m 0644 /tmp/hypervisor-fw.$$ "$FW_DEST"
    }
    if spin vm hypervisor-fw "fetching the Cloud Hypervisor firmware (boots cloud images)..." fetch_fw; then
      rm -f /tmp/hypervisor-fw.$$
      stepok vm "hypervisor-fw -> $FW_DEST"
    else
      rm -f /tmp/hypervisor-fw.$$
      warn "could not fetch rust-hypervisor-fw — `vm create` of a cloud image will need --firmware or --backend libvirt"
    fi
  else
    skip vm hypervisor-fw
  fi
  optional_dep vm virsh "libvirt-clients|libvirt-client|libvirt"                    "libvirt VM backend (fallback)"
  if ! command -v qemu-system-x86_64 >/dev/null 2>&1 && [ ! -e /usr/libexec/qemu-kvm ]; then
    step vm qemu-kvm "installing..."
    pkg_install "qemu-system-x86|qemu-kvm|qemu-base|qemu" >/dev/null 2>&1 \
      && stepok vm qemu-kvm || warn "could not install QEMU — libvirt VMs will not start"
  else
    skip vm qemu-kvm
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
      $SUDO usermod -aG "$grp" "$REAL_USER" && { stepok vm "group $grp ($REAL_USER added)"; NEED_RELOGIN=1; }
    fi
  done
  if [ ! -e /dev/kvm ]; then
    warn "/dev/kvm does not exist — hardware virtualization is off (enable VT-x/AMD-V in the BIOS) or you are in a VM without nested virt"
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
  step kernel modules "loading overlay/br_netfilter/tun..."
  printf '%s\n' overlay br_netfilter tun | $SUDO tee /etc/modules-load.d/delonix.conf >/dev/null
  for m in overlay br_netfilter tun; do $SUDO modprobe "$m" 2>/dev/null || true; done
  stepok kernel modules
  step kernel sysctls "applying (inotify/ip_forward/bridge-nf/max_map_count)..."
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
    warn "some sysctls did not apply (kernel without the module?) — they retry on next boot"
  fi
fi

# ------------------------------------------------- portas privilegiadas (opt-in)
# Publicar a porta 80/443 em rootless (`-p 80:80`, `kind: HTTPRoute` com
# `entrypoints: [{port: 80}]`) falha com `slirp_add_hostfwd failed`: quem liga a
# porta do lado do host é o slirp4netns, um processo SEM privilégios, e o kernel
# reserva as portas <1024 (`net.ipv4.ip_unprivileged_port_start`, 1024 por
# omissão). Não é limitação deste motor — o podman e o docker rootless têm o
# mesmo muro e documentam o mesmo contorno.
#
# DELIBERADAMENTE FORA do `--no-tune`/`99-delonix.conf` acima, e OFF por omissão:
# aqueles sysctls afinam limites (inotify, max_map_count) e não alteram nenhuma
# fronteira de privilégio; este BAIXA UMA, para o host inteiro — a partir daqui
# QUALQUER programa de QUALQUER utilizador local pode ligar-se às portas
# 80-1023, incluindo pôr-se à frente de um serviço que ainda não arrancou. Num
# portátil de desenvolvimento é um compromisso razoável; numa máquina partilhada
# ou de produção, a alternativa sem baixar nada é um proxy da porta 80 a correr
# como root (nginx/haproxy/systemd socket) a encaminhar para uma porta alta.
# Por isso: pedido explícito, ficheiro próprio (fácil de auditar e de reverter),
# e o valor a dizer exactamente o que abre.
if [ "$LOW_PORTS" = 1 ]; then
  CUR_LOW=$(sysctl -n net.ipv4.ip_unprivileged_port_start 2>/dev/null || echo 1024)
  if [ "$CUR_LOW" -le 80 ] 2>/dev/null; then
    skip kernel low-ports
  else
    step kernel low-ports "allowing unprivileged binds from port 80 (host-wide)..."
    $SUDO tee /etc/sysctl.d/99-delonix-lowports.conf >/dev/null <<'SYSCTL'
# Delonix Runtime — publicar portas <1024 em rootless (install.sh --low-ports).
# Baixa a fronteira de portas privilegiadas para TODO o host: qualquer programa
# de qualquer utilizador local passa a poder ligar-se a 80-1023.
# Reverter: rm este ficheiro + `sysctl -w net.ipv4.ip_unprivileged_port_start=1024`
net.ipv4.ip_unprivileged_port_start = 80
SYSCTL
    if $SUDO sysctl -q -p /etc/sysctl.d/99-delonix-lowports.conf >/dev/null 2>&1; then
      stepok kernel low-ports
      warn "unprivileged binds now start at port 80 for the WHOLE host (see /etc/sysctl.d/99-delonix-lowports.conf to revert)"
    else
      warn "could not apply net.ipv4.ip_unprivileged_port_start — it retries on next boot"
    fi
  fi
fi

# ------------------------------------------------------------ completion (bash)
if [ "$WITH_BINARY" = 1 ] && [ -d /etc/bash_completion.d ] && [ -x "$BIN_DIR/delonix" ]; then
  "$BIN_DIR/delonix" completion bash 2>/dev/null | $SUDO tee /etc/bash_completion.d/delonix >/dev/null \
    && stepok binary "bash completion" || true
fi

# ------------------------------------------------- delegação de cgroup (limites)
# SEM ISTO, `-m`/`--cpus`/`--pids-limit` são silenciosamente inertes.
#
# Medido numa VM limpa, por SSH normal, com `-m 128M --cpus 0.5`:
#
#     cgroup: /user.slice/user-1000.slice/session-40.scope   (partilhado com sshd)
#     memory.max=max  cpu.max=max  pids.max=max
#
# A causa é uma regra do cgroup v2, não um bug do motor: um scope de sessão SSH é
# IRMÃO de `user@<uid>.service`, não filho, e migrar um pid entre os dois exige
# escrever o `cgroup.procs` do antepassado comum (`user-<uid>.slice`), que é da
# root. O motor avisa quando isto acontece, mas um aviso não é um limite.
#
# `enable-linger` garante que o `user@<uid>.service` existe e persiste mesmo sem
# sessão aberta — é o pré-requisito para qualquer uso não interactivo (cron, CI,
# um serviço de utilizador). É o mesmo requisito que o Podman rootless tem.
if command -v loginctl >/dev/null 2>&1 && [ -n "$REAL_USER" ]; then
  if loginctl show-user "$REAL_USER" -p Linger 2>/dev/null | grep -q 'Linger=yes'; then
    skip rootless linger
  else
    step rootless linger "enabling systemd lingering for $REAL_USER..."
    $SUDO loginctl enable-linger "$REAL_USER" 2>/dev/null \
      && stepok rootless linger \
      || step rootless linger "could not enable (non-fatal; affects non-interactive use only)"
  fi
fi

# ----------------------------------------------------------------- verificação
msg "verifying the installation..."
FAIL=0
check() { # $1 descrição, $2.. comando
  local desc="$1"; shift
  if "$@" >/dev/null 2>&1; then stepok verify "$desc"; else
    printf '[verify] %s: %sFAILED%s\n' "$desc" "$C_ERR" "$C_0"; FAIL=1
  fi
}
[ "$WITH_BINARY" = 1 ] && check "delonix ($("$BIN_DIR/delonix" --version 2>/dev/null || echo '?'))" "$BIN_DIR/delonix" --version
check "slirp4netns"                    has_cmd slirp4netns
check "newuidmap"                      has_cmd newuidmap
check "newuidmap privileged"           sh -c 'nm=$(command -v newuidmap) && { [ -u "$nm" ] || getcap "$nm" 2>/dev/null | grep -q cap_setuid; }'
check "nft"                            has_cmd nft
check "subuid range for $REAL_USER"    grep -q "^$REAL_USER:" /etc/subuid
check "subgid range for $REAL_USER"    grep -q "^$REAL_USER:" /etc/subgid
check "/dev/net/tun"                   test -e /dev/net/tun
check "user namespaces"                unshare -r -n true
if [ "$WITH_VM" = 1 ]; then
  check "VM backend (cloud-hypervisor or virsh)" sh -c 'command -v cloud-hypervisor || command -v virsh'
fi

# Testa a delegação DE VERDADE — cria um cgroup filho e tenta activar os
# controladores. `cat /sys/fs/cgroup/.../cgroup.controllers` não chega: o
# controlador pode estar listado e a migração continuar proibida.
CGROUP_DELEGATED=0
if [ "$(stat -fc %T /sys/fs/cgroup 2>/dev/null)" = cgroup2fs ]; then
  _cg=$(sed -n 's|^0::||p' /proc/self/cgroup 2>/dev/null)
  _probe="/sys/fs/cgroup${_cg}/.delonix-probe"
  if mkdir -p "$_probe" 2>/dev/null &&
     printf '+memory' > "/sys/fs/cgroup${_cg}/cgroup.subtree_control" 2>/dev/null; then
    CGROUP_DELEGATED=1
    printf '\055memory' > "/sys/fs/cgroup${_cg}/cgroup.subtree_control" 2>/dev/null || true
  fi
  rmdir "$_probe" 2>/dev/null || true
fi
if [ "$CGROUP_DELEGATED" = 1 ]; then
  stepok verify "cgroup delegation (resource limits apply)"
else
  printf '[verify] %s: %sNOT DELEGATED%s\n' "cgroup delegation" "$C_WARN" "$C_0"
fi

echo
if [ "$FAIL" = 0 ]; then
  msg "ready"
  echo "    delonix container run -d -p 8080:80 nginx && curl localhost:8080"
else
  warn "installation finished with warnings — review the FAILED lines above"
fi

if [ "$CGROUP_DELEGATED" != 1 ]; then
  echo
  warn "resource limits (-m / --cpus / --pids-limit) will NOT be applied from this shell"
  cat <<'CGHINT'
    This shell has no delegated cgroup, so the engine cannot create one to put a
    container in. It is a cgroup v2 rule, not a limitation of Delonix: an SSH
    session scope is a SIBLING of user@<uid>.service, and moving a process
    between them needs write access to a cgroup owned by root. Rootless Podman
    has exactly the same requirement.

    Namespace and seccomp isolation are unaffected — only the resource ceilings.

    Run workloads inside a delegated scope:

        systemd-run --user --scope -p Delegate=yes -- delonix container run ...

    …or, for anything long-lived, from a systemd USER unit (which already gets a
    delegated cgroup):

        systemctl --user edit --force --full delonix-app.service
        # [Service]
        # Delegate=yes
        # ExecStart=/usr/local/bin/delonix container run ...

    Verify it took effect:

        systemd-run --user --scope -p Delegate=yes -- \
          delonix container run -d --name t -m 128M alpine sleep 60
        # memory.max should read 134217728, not "max"
CGHINT
fi
if [ "$NEED_RELOGIN" = 1 ]; then
  warn "log out and back in (or run 'newgrp kvm') for the new group memberships to take effect"
fi

}  # fim do bloco de protecção contra download truncado (ver o topo do ficheiro)
