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
#   --no-gpu       não configura aceleradores (CDI da NVIDIA, grupo render).
#                  Por omissão é LIGADO, e só faz algo se houver GPU.
#   --no-binary    só dependências/configuração (usa um binário já instalado)
#   --no-editor-plugin  não instala a extensão nos editores VS Code encontrados
#   --with-cri     instala também o delonix-cri (nó Kubernetes)
#   --low-ports    permite publicar portas <1024 (ex.: 80/443) sem root.
#   --with-image-build
#                  instala o que CONSTRUIR imagens VM exige (libguestfs) e
#                  torna /boot/vmlinuz-* legivel. Ver a seccao dedicada: o
#                  chmod baixa uma fronteira de seguranca do host.
#   --production   tuning de ESCALA para um no de producao (conntrack, ARP,
#                  portas efemeras, fds, pids). Ver a seccao dedicada.
#   --no-delegate  NÃO escreve o drop-in de delegação de cgroup (ver abaixo).
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
# Delegação de cgroup ligada POR OMISSÃO: sem ela `-m`/`--cpus`/`--pids-limit`
# são silenciosamente inertes e um nó Kubernetes nem arranca. Instalar um motor
# de containers cujos limites não pegam é entregar metade do produto.
WITH_DELEGATE=1
WITH_TUNE=1
# Fase de aceleradores: ligada por omissao, mas inerte num host sem GPU — ver
# a seccao "aceleradores" mais abaixo, que tem tres estados e nao dois.
WITH_GPU=1
WITH_BINARY=1
WITH_CRI=0
WITH_EDITOR_PLUGIN=1
USER_INSTALL=0
LOW_PORTS=0
WITH_IMAGE_BUILD=0
PRODUCTION=0
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
    --no-gpu)     WITH_GPU=0 ;;
    --no-editor-plugin) WITH_EDITOR_PLUGIN=0 ;;
    --no-binary)  WITH_BINARY=0 ;;
    --with-cri)   WITH_CRI=1 ;;
    --low-ports)  LOW_PORTS=1 ;;
    --with-image-build) WITH_IMAGE_BUILD=1 ;;
    --production) PRODUCTION=1 ;;
    --no-delegate) WITH_DELEGATE=0 ;;
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
fi
# A autenticação em si (`sudo -v`) NÃO corre aqui — ver a nota mais abaixo,
# a seguir ao download/instalação do binário. `--user` sem root nenhum é um
# caso de uso deliberado (só o binário, em ~/.local/bin) e não pode ficar
# refém de dependências que nem sequer vão ser tocadas.

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
  # Um asset cujo nome é o nome, sem sufixo de arquitectura. O `fetch_asset`
  # acima existe para BINÁRIOS e compõe sempre `-x86_64[-v3]-linux`; um `.vsix`
  # é independente de arquitectura e chama-se `delonix-vscode` e mais nada.
  # Reutilizar aquele aqui pedia dois nomes que não existem, levava 404 nos dois,
  # e reportava «this release ships no editor extension» sobre uma release que a
  # traz — medido contra a v0.66.0.
  fetch_named_asset() { # $1 nome EXACTO do asset
    curl -fsSL -o "$TMP/$1" "$BASE_URL/$1" || return 1
    echo "$1"
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
  # ---- Aviso de migração 0.64 -> 0.65+ ------------------------------------
  # A v0.64.0 diz, em três sítios, «nada é obrigatório; os manifestos
  # existentes carregam sem alteração». A v0.65.0 tornou isso falso no MESMO
  # dia: removeu `kind: Storage`, `ShareVolume` e `Egress`. Quem leu as notas da
  # 0.64 e adiou a migração tinha razão nesse dia e fica preso neste upgrade.
  #
  # O aviso corre ANTES de o binário ser substituído — depois já não há como
  # saber de onde se veio — e só quando se vem MESMO de uma 0.64.x, para não
  # ser ruído numa instalação nova.
  PREV_VER=$(command -v delonix >/dev/null 2>&1 && delonix --version 2>/dev/null | head -1 | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | head -1 || true)
  case "$PREV_VER" in
    0.64.*)
      warn "upgrading from $PREV_VER: three Kinds were REMOVED in 0.65.0 and manifests using them stop loading."
      warn "  kind: Storage      -> kind: Volume with an nfs:/cifs:/webdav: block"
      warn "  kind: ShareVolume  -> kind: Volume with a share: block"
      warn "  kind: Egress       -> kind: NetworkPolicy with direction: egress"
      warn "  migration: https://angolardevops.github.io/delonix-runtime/estrutura.html"
      ;;
  esac
  # Guarda explícita: sem `--user` isto é a 1ª chamada a sudo do script (a
  # autenticação eager foi adiada para depois desta secção — ver a nota mais
  # abaixo). Sem `|| die`, um sudo sem TTY/credencial cache abortava aqui por
  # `set -e` com o stderr cru do sudo, em vez de um erro claro e accionável.
  $BIN_SUDO install -m 0755 "$TMP/$DELONIX_ASSET" "$BIN_DIR/delonix" \
    || die "could not install the delonix binary to $BIN_DIR — sudo failed or the destination isn't writable (use --user for a no-root install to \$HOME/.local/bin, or ensure sudo works non-interactively)"
  stepok binary "delonix -> $BIN_DIR/delonix"
  if [ "$WITH_CRI" = 1 ]; then
    dl_cri() { fetch_asset delonix-cri > "$TMP/.asset-cri"; }
    spin binary delonix-cri "downloading..." dl_cri \
      || die "delonix-cri download failed"
    CRI_ASSET=$(cat "$TMP/.asset-cri")
    verify_asset "$CRI_ASSET"
    $BIN_SUDO install -m 0755 "$TMP/$CRI_ASSET" "$BIN_DIR/delonix-cri" \
      || die "could not install delonix-cri to $BIN_DIR — sudo failed or the destination isn't writable"
    stepok binary "delonix-cri -> $BIN_DIR/delonix-cri"
  fi
  case ":$PATH:" in *":$BIN_DIR:"*) ;; *) warn "$BIN_DIR is not in your PATH" ;; esac
  # Um delonix ANTIGO mais à frente no PATH faz sombra ao acabado de instalar
  # (caso real: um build 0.3.0 em ~/.local/bin escondia o 0.4.2 e ressuscitava
  # bugs já corrigidos). Detectar e dizer alto qual apagar.
  # ---- Extensão de editor -------------------------------------------------
  # Um só caminho serve VS Code E os seus forks: o Cursor, o Windsurf, o
  # VSCodium e o Antigravity partilham a CLI de extensões, verificado neste host
  # (`antigravity --list-extensions` responde tal como o `code`).
  #
  # O `codex` NÃO entra: é um agente de linha de comandos da OpenAI, não um
  # editor com extensões — não há onde instalar um `.vsix`.
  #
  # BEST-EFFORT, e a razão é a mesma da etiqueta de GPU que já chumbou este
  # script num host sem VGA: uma conveniência não pode falhar a instalação do
  # motor. Cada editor é tentado, uma falha avisa e segue.
  if [ "$WITH_EDITOR_PLUGIN" = 1 ]; then
    EDITORS_FOUND=""
    for ed in code code-insiders codium cursor windsurf antigravity; do
      command -v "$ed" >/dev/null 2>&1 && EDITORS_FOUND="$EDITORS_FOUND $ed"
    done
    if [ -z "$EDITORS_FOUND" ]; then
      skip editor "no VS Code-family editor on this host"
    else
      # Sem `2>/dev/null`: foi silenciar este stderr que escondeu o 404 e fez o
      # SKIP abaixo afirmar uma coisa falsa durante uma release inteira.
      dl_vsix() { fetch_named_asset delonix-vscode > "$TMP/.asset-vsix"; }
      if spin editor plugin "downloading..." dl_vsix; then
        VSIX_ASSET=$(cat "$TMP/.asset-vsix")
        verify_asset "$VSIX_ASSET"
        # O CLI do VS Code EXIGE que o ficheiro termine em `.vsix`: sem isso lê o
        # argumento como um ID de extensão e responde «make sure you use the full
        # extension ID, including the publisher», uma frase que não tem nada que
        # ver com a causa. O asset chama-se `delonix-vscode` porque é esse o nome
        # que o SHA256SUMS assinado cobre — por isso verifica-se com o nome dele
        # e instala-se com uma cópia que tem a extensão que o editor pede.
        cp "$TMP/$VSIX_ASSET" "$TMP/$VSIX_ASSET.vsix"
        for ed in $EDITORS_FOUND; do
          # NUNCA sobrepor uma extensão já instalada. A release do motor traz a
          # versão que existia quando ela foi construída, e essa pode ser MAIS
          # VELHA do que a que o editor tem — foi o que aconteceu neste host:
          # um `--force` cego trocou a 0.2.0 pela 0.1.0 e levou a árvore de
          # recursos com ele, sem uma palavra. Quando a extensão estiver nas
          # galerias, é o editor que a mantém em dia; o trabalho deste script é
          # a PRIMEIRA instalação, e mais nenhum.
          # `|| true`: um `grep` sem correspondência sai 1 e, sob `set -e` com
          # `pipefail`, a atribuição inteira falha — o script morria em silêncio
          # AQUI, no caso mais comum que existe (ainda não ter a extensão). É o
          # mesmo defeito que a etiqueta de GPU já custou a este ficheiro, e só
          # apareceu com um editor de teste sem extensões: contra um editor que
          # JÁ a tinha, o grep casava e nada se via.
          HAVE=$("$ed" --list-extensions --show-versions 2>/dev/null \
                   | grep -i '^angolardevops\.delonix@' | head -1 || true)
          if [ -n "$HAVE" ]; then
            # A frase NÃO promete que o editor a actualiza: isso só passa a ser
            # verdade quando a extensão estiver nas galerias, e hoje não está.
            # Diz o que é verdade em qualquer dos dois estados.
            skip editor "$ed already has ${HAVE#*@} — leaving it alone (update it from the editor, or install a .vsix by hand)"
            continue
          fi
          if "$ed" --install-extension "$TMP/$VSIX_ASSET.vsix" --force >/dev/null 2>&1; then
            stepok editor "$ed"
          else
            warn "could not install the extension into $ed — install it by hand: $ed --install-extension <the .vsix from the release>"
          fi
        done
      else
        # Uma release anterior à extensão não traz o asset. Não é uma falha do
        # host, e dizer «download failed» mandaria alguém procurar rede partida.
        skip editor "this release ships no editor extension"
      fi
    fi
  fi

  ACTIVE=$(command -v delonix 2>/dev/null || true)
  if [ -n "$ACTIVE" ] && [ "$ACTIVE" != "$BIN_DIR/delonix" ]; then
    warn "another delonix shadows the one just installed: '$ACTIVE' ($("$ACTIVE" --version 2>/dev/null || echo unknown version)) comes first in PATH — remove it (rm $ACTIVE) to use $BIN_DIR/delonix"
  fi
else
  BIN_DIR=$(dirname "$(command -v delonix 2>/dev/null || echo /usr/local/bin/delonix)")
fi

# BUG REAL corrigido aqui: `sudo -v` corria logo no arranque do script,
# incondicionalmente para qualquer utilizador não-root — ANTES sequer de
# saber se alguma coisa ia precisar de root. Um `--user` com todas as
# dependências já satisfeitas (o caso normal de voltar a correr o instalador
# só para apanhar uma release nova) morria em "sudo authentication failed"
# sem chegar a tocar no binário — reproduzido ao vivo (sem TTY/sudo cache):
# o binário ficava na versão antiga, mesmo com o download/verificação/
# instalação em si a funcionarem perfeitamente quando testados isolados.
# Adiar a autenticação para AQUI — depois do binário já descarregado,
# verificado e instalado — mantém a garantia original ("um `pkg_install`
# falhado adiante significa mesmo 'pacote indisponível', nunca 'sudo falhou
# em silêncio'") para tudo o que se segue, e deixa de bloquear o único
# caminho que não precisa de root nenhum.
if [ -n "$SUDO" ]; then
  msg "some steps need root — sudo may ask for your password"
  sudo -v || die "sudo authentication failed — run again and enter your password, or run as root"
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
  # ESTÁTICO oficial do upstream. O upstream não publica um SHA256SUMS nem
  # assinatura (ao contrário das nossas próprias releases, ver acima) — por
  # isso os três firmwares desta secção são PINADOS por nós: versão/tag fixa
  # (nunca `releases/latest`) + hash calculado uma vez e verificado aqui.
  # Actualizar exige mudar os dois — versão e hash — no MESMO commit, a
  # mesma disciplina do `lang_ratchet.py --update`. Sem isto, um upstream
  # comprometido ou um TLS-stripping serviriam um binário diferente sem
  # detecção nenhuma — achado MÉDIO da auditoria de segurança #2.
  CH_STATIC_VERSION="v53.0"
  CH_STATIC_SHA256="448af3d4e59b22c2987f7df94c213ad40fb53a10d437e42b5ee6c4fce7c29ecc"
  EDK2_TAG="ch-f308d878a6"
  EDK2_SHA256="edd3ceb8de672ec4317a9d68de1f5edc9f48ef2c0283853c7c681332573ff46a"
  HYPFW_VERSION="0.5.0"
  HYPFW_SHA256="4a0a1e977368f6b15d2198a216bdedf9a350bf5e5ae07e29e695373ec16ad958"
  verify_pinned_sha256() { # $1=ficheiro local  $2=hash esperado
    [ "$(sha256sum "$1" | awk '{print $1}')" = "$2" ]
  }
  if ! command -v cloud-hypervisor >/dev/null 2>&1; then
    if pkg_install cloud-hypervisor >/dev/null 2>&1; then
      stepok vm cloud-hypervisor
    else
      CH_URL="https://github.com/cloud-hypervisor/cloud-hypervisor/releases/download/$CH_STATIC_VERSION/cloud-hypervisor-static"
      fetch_ch() {
        curl -fsSL -o /tmp/cloud-hypervisor-static.$$ "$CH_URL" \
          && verify_pinned_sha256 /tmp/cloud-hypervisor-static.$$ "$CH_STATIC_SHA256" \
          && $SUDO install -m 0755 /tmp/cloud-hypervisor-static.$$ /usr/local/bin/cloud-hypervisor
      }
      if spin vm cloud-hypervisor "not packaged on this distro — fetching the pinned static binary ($CH_STATIC_VERSION)..." fetch_ch; then
        rm -f /tmp/cloud-hypervisor-static.$$
        stepok vm "cloud-hypervisor -> /usr/local/bin/cloud-hypervisor ($(/usr/local/bin/cloud-hypervisor --version 2>/dev/null | head -1))"
      else
        rm -f /tmp/cloud-hypervisor-static.$$
        warn "could not fetch/verify cloud-hypervisor $CH_STATIC_VERSION (checksum mismatch means a tampered or corrupted download — not installed) — the libvirt backend below remains the fallback"
      fi
    fi
  else
    skip vm cloud-hypervisor
  fi
  # Firmware do Cloud Hypervisor: o CH não tem BIOS, por isso uma cloud image
  # (a golden `delonix vm pull`) só arranca com firmware. São instalados DOIS,
  # e o motor prefere o EDK2 (ver `default_ch_firmware`).
  #
  # PORQUÊ os dois, medido em 2026-08-12 e não suposto: sob o `hypervisor-fw`
  # NENHUMA imagem deste projecto arrancava em CH — as `delonix-vm-base:*`
  # ficavam com o overlay a 448 KiB (o SO nunca escreveu nada) e a golden k8s
  # morria no shim de Secure Boot (`import_mok_state() failed: Unsupported`).
  # Com o EDK2 `CLOUDHV.fd` arrancam e ganham IP na SDN: ubuntu-24.04 em 7,8s,
  # ubuntu-26.04 e debian-bookworm em 5s, rocky-9 em 32s, a golden em 7s.
  # O `hypervisor-fw` continua a ser instalado como recurso (é ~150 KB, arranca
  # mais depressa onde funcione, e tirá-lo mudaria o comportamento de uma VM
  # que hoje dependa dele).
  EDK2_DEST=/usr/local/share/delonix/CLOUDHV.fd
  if [ ! -e "$EDK2_DEST" ]; then
    EDK2_URL="https://github.com/cloud-hypervisor/edk2/releases/download/$EDK2_TAG/CLOUDHV.fd"
    fetch_edk2() {
      $SUDO mkdir -p /usr/local/share/delonix \
        && curl -fsSL -o /tmp/CLOUDHV.fd.$$ "$EDK2_URL" \
        && verify_pinned_sha256 /tmp/CLOUDHV.fd.$$ "$EDK2_SHA256" \
        && $SUDO install -m 0644 /tmp/CLOUDHV.fd.$$ "$EDK2_DEST"
    }
    if spin vm CLOUDHV.fd "fetching the EDK2 firmware for Cloud Hypervisor ($EDK2_TAG)..." fetch_edk2; then
      rm -f /tmp/CLOUDHV.fd.$$
      stepok vm "CLOUDHV.fd -> $EDK2_DEST"
    else
      rm -f /tmp/CLOUDHV.fd.$$
      warn "could not fetch/verify the EDK2 firmware $EDK2_TAG (checksum mismatch means a tampered or corrupted download — not installed) — a Cloud Hypervisor VM will fall back to rust-hypervisor-fw, which boots none of the delonix images (use --backend libvirt)"
    fi
  else
    skip vm CLOUDHV.fd
  fi
  FW_DEST=/usr/local/share/delonix/hypervisor-fw
  if [ ! -e "$FW_DEST" ]; then
    FW_URL="https://github.com/cloud-hypervisor/rust-hypervisor-firmware/releases/download/$HYPFW_VERSION/hypervisor-fw"
    fetch_fw() {
      $SUDO mkdir -p /usr/local/share/delonix \
        && curl -fsSL -o /tmp/hypervisor-fw.$$ "$FW_URL" \
        && verify_pinned_sha256 /tmp/hypervisor-fw.$$ "$HYPFW_SHA256" \
        && $SUDO install -m 0644 /tmp/hypervisor-fw.$$ "$FW_DEST"
    }
    if spin vm hypervisor-fw "fetching the Cloud Hypervisor firmware ($HYPFW_VERSION)..." fetch_fw; then
      rm -f /tmp/hypervisor-fw.$$
      stepok vm "hypervisor-fw -> $FW_DEST"
    else
      rm -f /tmp/hypervisor-fw.$$
      warn "could not fetch/verify rust-hypervisor-fw $HYPFW_VERSION (checksum mismatch means a tampered or corrupted download — not installed) — `vm create` of a cloud image will need --firmware or --backend libvirt"
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

# ------------------------------------------------ aceleradores (GPU), opt-out
# Ate aqui a GPU era uma ETIQUETA: o `step host gpu` la em cima corre um lspci,
# imprime o nome da placa e nao configura NADA. O motor, esse, exige uma spec
# CDI para `--gpus nvidia|all` e para um `--device nvidia.com/gpu=...`, e sem
# ela recusa a corrida com uma mensagem a mandar o utilizador instalar o
# nvidia-container-toolkit e correr `nvidia-ctk cdi generate` A MAO. Dois
# passos manuais no caminho normal de quem so quer correr um container com
# GPU — exactamente o que este instalador existe para nao deixar acontecer.
#
# A fase tem TRES estados por acelerador, e nunca dois:
#   configurado          — ja esta, salta (SKIP)
#   presente mas nao configurado — configura, e diz o que fez
#   ausente              — salta EM SILENCIO. Uma maquina sem GPU nao leva um
#                          aviso amarelo por nao ter uma GPU.
#
# Porque nao vem do gestor de pacotes da distro: o nvidia-container-toolkit nao
# esta nos repositorios do Debian/Ubuntu/Fedora — vem do repositorio da propria
# NVIDIA, e por isso este bloco acrescenta-o (chave + lista) antes de instalar.
# Numa distro cujo repositorio nao publicamos aqui, NAO se inventa: escreve-se
# o comando exacto e segue-se.
#
# AMD e Intel nao precisam de CDI nenhum: `--gpus dri` liga /dev/dri/renderD*
# em cru e o que falta e quase sempre so a pertenca ao grupo `render`.
NEED_GPU_PROOF=0
if [ "$WITH_GPU" = 1 ]; then
  # Deteccao pelo NO, nao pelo lspci: um driver instalado sem modulo carregado
  # nao serve para nada, e /dev/nvidia0 so existe quando o modulo esta de pe.
  HAS_NVIDIA_NODE=0
  ls /dev/nvidia[0-9]* >/dev/null 2>&1 && HAS_NVIDIA_NODE=1
  HAS_NVIDIA_PCI=0
  if command -v lspci >/dev/null 2>&1; then
    lspci -n 2>/dev/null | awk '{print $3}' | grep -qi '^10de:' && HAS_NVIDIA_PCI=1
  fi
  HAS_DRI=0
  ls /dev/dri/renderD* >/dev/null 2>&1 && HAS_DRI=1

  # `ls a b` sai 2 quando UM dos globos nao casa, mesmo tendo listado o outro —
  # e com quatro caminhos a verificar isso dava sempre "nao ha spec", que fazia
  # o instalador regerar por cima de uma spec ja gerada pelo nvidia-cdi-refresh.
  cdi_spec_path() {
    local d f
    for d in /etc/cdi /var/run/cdi; do
      for f in "$d"/*.yaml "$d"/*.yml "$d"/*.json; do
        [ -e "$f" ] && { printf '%s\n' "$f"; return 0; }
      done
    done
    return 1
  }

  if [ "$HAS_NVIDIA_NODE" = 0 ] && [ "$HAS_NVIDIA_PCI" = 1 ]; then
    # Placa no barramento, sem no em /dev: o driver nao esta instalado ou o
    # modulo nao carregou. Nao e este instalador que instala drivers de kernel
    # proprietarios — mas cala-lo seria pior, porque `--gpus all` vai falhar.
    warn "NVIDIA GPU on the bus but no /dev/nvidia* node — the driver is not installed or the module did not load; install your distro's NVIDIA driver, reboot, and re-run this installer to finish the GPU setup"
  fi

  if [ "$HAS_NVIDIA_NODE" = 1 ]; then
    # 1. o toolkit. So ele sabe que bibliotecas o driver INSTALADO exporta —
    #    a versao muda a cada actualizacao e nao se adivinha.
    if command -v nvidia-ctk >/dev/null 2>&1; then
      skip accel nvidia-ctk
    else
      step accel nvidia-ctk "adding the NVIDIA repository and installing nvidia-container-toolkit..."
      _ctk_ok=0
      case "$PKG" in
        apt)
          if curl -fsSL https://nvidia.github.io/libnvidia-container/gpgkey 2>/dev/null \
               | $SUDO gpg --batch --yes --dearmor -o /usr/share/keyrings/nvidia-container-toolkit-keyring.gpg 2>/dev/null \
             && curl -fsSL https://nvidia.github.io/libnvidia-container/stable/deb/nvidia-container-toolkit.list 2>/dev/null \
               | sed 's#deb https://#deb [signed-by=/usr/share/keyrings/nvidia-container-toolkit-keyring.gpg] https://#g' \
               | $SUDO tee /etc/apt/sources.list.d/nvidia-container-toolkit.list >/dev/null; then
            PKG_UPDATED=0   # a lista e nova: o indice tem mesmo de ser relido
            pkg_update_once
            pkg_install nvidia-container-toolkit >/dev/null 2>&1 && _ctk_ok=1
          fi ;;
        dnf)
          if curl -fsSL https://nvidia.github.io/libnvidia-container/stable/rpm/nvidia-container-toolkit.repo 2>/dev/null \
               | $SUDO tee /etc/yum.repos.d/nvidia-container-toolkit.repo >/dev/null; then
            pkg_install nvidia-container-toolkit >/dev/null 2>&1 && _ctk_ok=1
          fi ;;
        *)
          # Nao publicamos o repositorio desta distro. O comando exacto vale
          # mais do que uma tentativa as cegas com o nome errado do pacote.
          warn "install nvidia-container-toolkit from your distro (Arch: pacman -S nvidia-container-toolkit; openSUSE: see https://github.com/NVIDIA/nvidia-container-toolkit), then re-run this installer" ;;
      esac
      if [ "$_ctk_ok" = 1 ] && command -v nvidia-ctk >/dev/null 2>&1; then
        stepok accel nvidia-ctk
      elif [ "$_ctk_ok" = 1 ]; then
        warn "nvidia-container-toolkit installed but nvidia-ctk is not on PATH"
      else
        warn "could not install nvidia-container-toolkit — 'delonix container run --gpus all' will refuse to start until it exists"
      fi
    fi

    # 2. a spec CDI. E ela que o motor le; o toolkit sozinho nao chega, e a
    #    versao 1.17+ do pacote instala um nvidia-cdi-refresh.path que a
    #    regenera em /var/run/cdi a cada boot e a cada troca de driver. Se ele
    #    ja a escreveu, nao se escreve uma segunda em /etc/cdi: duas specs do
    #    mesmo vendor sao lidas as DUAS, e a mais velha sobrevive a uma
    #    actualizacao de driver a apontar para bibliotecas que ja nao existem.
    if cdi_spec_path >/dev/null; then
      skip accel cdi-spec
    elif command -v nvidia-ctk >/dev/null 2>&1; then
      step accel cdi-spec "generating /etc/cdi/nvidia.yaml..."
      $SUDO mkdir -p /etc/cdi
      if $SUDO nvidia-ctk cdi generate --output=/etc/cdi/nvidia.yaml >/dev/null 2>&1; then
        stepok accel cdi-spec
      else
        warn "nvidia-ctk could not generate the CDI spec — run 'sudo nvidia-ctk cdi generate --output=/etc/cdi/nvidia.yaml' and read its error"
      fi
    fi

    # 3. a spec e util? Uma que nao declare os nos de CONTROLO nao serve: com
    #    /dev/nvidia0 e sem /dev/nvidiactl + /dev/nvidia-uvm, nenhum processo
    #    CUDA inicializa. Vale a pena olhar, porque o modo de falha e um
    #    container que arranca bem e so estoura quando alguem chama a GPU.
    _cdi_spec=$(cdi_spec_path || true)
    if [ -n "$_cdi_spec" ]; then
      if grep -q 'nvidiactl' "$_cdi_spec" 2>/dev/null && grep -q 'nvidia-uvm' "$_cdi_spec" 2>/dev/null; then
        stepok accel gpu-ready
        NEED_GPU_PROOF=1
      else
        warn "the CDI spec at $_cdi_spec does not declare /dev/nvidiactl and /dev/nvidia-uvm — containers will see the GPU node but CUDA will not initialise; regenerate it with 'sudo nvidia-ctk cdi generate'"
      fi
    fi
  fi

  # AMD/Intel (e a parte /dev/dri de uma NVIDIA): sem CDI, so pertenca de
  # grupo. `--gpus dri` liga os nos em cru e e o suficiente para VA-API,
  # Vulkan e ROCm.
  if [ "$HAS_DRI" = 1 ]; then
    _dri_grp=$(stat -c '%G' /dev/dri/renderD128 2>/dev/null || echo render)
    if id -nG "$REAL_USER" 2>/dev/null | tr ' ' '\n' | grep -qx "$_dri_grp"; then
      skip accel dri-group
    elif getent group "$_dri_grp" >/dev/null 2>&1; then
      step accel dri-group "adding $REAL_USER to '$_dri_grp' (for --gpus dri)..."
      $SUDO usermod -aG "$_dri_grp" "$REAL_USER" 2>/dev/null \
        && { stepok accel dri-group; NEED_RELOGIN=1; } \
        || warn "could not add $REAL_USER to '$_dri_grp' — '--gpus dri' will fail with EACCES"
    fi
  fi

  # `[ ... ] && skip ...` seria mais curto e ABORTARIA o instalador: com a
  # condicao falsa o comando devolve 1 e o `set -e` la de cima mata o processo
  # a meio, sem uma linha de erro. Foi assim que a etiqueta de GPU ja matou uma
  # instalacao inteira (ver o comentario do GPU_INFO). Portanto: `if`.
  if [ "$HAS_NVIDIA_NODE" = 0 ] && [ "$HAS_NVIDIA_PCI" = 0 ] && [ "$HAS_DRI" = 0 ]; then
    skip accel gpu
  fi
fi

# ------------------------------------- construir imagens VM (opt-in explicito)
# `delonix image --vm build` corre `virt-customize`, que constroi um appliance
# com o supermin. Duas coisas que faltam por omissao num host tipico, e cada uma
# falha de forma que ninguem adivinha:
#
#   isc-dhcp-client — o supermin.d/packages do libguestfs pede-o e o supermin so
#     COPIA pacotes do host: sem ele instalado AQUI, o appliance nasce sem
#     cliente DHCP, nunca obtem IP nem DNS, e o build morre em
#     "Temporary failure resolving 'archive.ubuntu.com'" — um erro que parece de
#     rede do host, e o host tem rede.
#
#   /boot/vmlinuz-* legivel — o supermin copia o kernel do host para o
#     appliance. Debian/Ubuntu instalam-no 0600, e o build morre em
#     "cp: cannot open '/boot/vmlinuz-...' for reading: Permission denied".
#
# O chmod BAIXA UMA FRONTEIRA: o binario do kernel passa a ser legivel por
# qualquer utilizador local (o 0600 existe para dificultar exploits que
# beneficiam de conhecer o kernel exacto). Por isso e opt-in, avisa, e diz como
# reverter — o mesmo tratamento que `--low-ports` ja tem.
if [ "$WITH_IMAGE_BUILD" = 1 ]; then
  optional_dep imgbuild virt-customize "libguestfs-tools|guestfs-tools" "building VM images"
  optional_dep imgbuild dhclient "isc-dhcp-client|dhcp-client|dhcp" "network in the libguestfs appliance"
  UNREADABLE_KERNEL=0
  for k in /boot/vmlinuz-*; do
    [ -e "$k" ] || continue
    [ -r "$k" ] || UNREADABLE_KERNEL=1
  done
  if [ "$UNREADABLE_KERNEL" = 1 ]; then
    step imgbuild kernel-readable "making /boot/vmlinuz-* readable (supermin copies it)..."
    if $SUDO chmod 0644 /boot/vmlinuz-* 2>/dev/null; then
      stepok imgbuild kernel-readable
      warn "the host kernel is now world-READABLE (revert: sudo chmod 0600 /boot/vmlinuz-*)"
    else
      warn "could not chmod /boot/vmlinuz-* — `image --vm build` will fail in supermin"
    fi
  else
    skip imgbuild kernel-readable
  fi

  # ---- passt: o libguestfs usa-o para dar rede ao appliance, e o empacotado
  # do Ubuntu 24.04 ARRANCA e nunca atribui lease. O que se ve nao tem a
  # palavra passt em lado nenhum: o dhclient espera ~300s, o build segue SEM
  # rede, e falha la a frente com o gestor de pacotes a nao resolver um
  # mirror — le-se como "o teu DNS esta partido". Medido outra vez a
  # 2026-08-18, a construir a delonix-vm-base.
  #
  # O remedio ja estava escrito na mensagem de erro do motor, como PASSO
  # MANUAL. Um passo manual no caminho de quem instala e um bloqueio, por isso
  # e feito aqui: compila-se o passt actual para /usr/local/bin, que precede
  # /usr/bin no PATH — o libguestfs procura-o por PATH e passa a achar este.
  #
  # So quando ha um passt do SISTEMA e nao ha ja um em /usr/local/bin: nao se
  # instala o que ninguem usa, e nao se pisa uma build que o operador tenha
  # posto la. Falha ABERTO (warn e segue): um passt velho da um build sem rede
  # com erro claro, um instalador que morre aqui nao instala nada.
  if [ -x /usr/local/bin/passt ]; then
    skip imgbuild passt
  elif ! has_cmd passt; then
    skip imgbuild passt
  else
    step imgbuild passt "building a current passt (the packaged one never leases)..."
    PASST_SRC="$(mktemp -d)"
    if command -v git >/dev/null 2>&1 && command -v make >/dev/null 2>&1 && command -v cc >/dev/null 2>&1 \
       && git clone -q --depth 1 https://passt.top/passt "$PASST_SRC/passt" >/dev/null 2>&1 \
       && ( cd "$PASST_SRC/passt" && make -j"$(nproc 2>/dev/null || echo 2)" >/dev/null 2>&1 ) \
       && $SUDO install -m 0755 "$PASST_SRC/passt/passt" /usr/local/bin/passt 2>/dev/null; then
      stepok imgbuild passt
    else
      warn "could not build passt — a network image build may fail with a DNS error;\
 see https://passt.top/passt (build it and put it first in PATH)"
    fi
    rm -rf "$PASST_SRC"
  fi
fi

# --------------------------------------- tuning de ESCALA (--production, opt-in)
# O bloco `99-delonix.conf` acima da correccao do que FALHA num host normal.
# Este da os limites que so se atingem em CARGA, e cada um foi escolhido por um
# modo de falha concreto e diagnosticavel — nada de folclore:
#
#   nf_conntrack_max — TODO o dataplane deste motor e nftables com conntrack
#     (o `ct state` das chains por-workload, o NAT do slirp). Cheio, o kernel
#     DROPA ligacoes novas e escreve "nf_conntrack: table full" no dmesg; do
#     lado da aplicacao parece perda de pacotes aleatoria.
#   neigh gc_thresh — a tabela ARP tem 1024 entradas por omissao. Um no com
#     muitas centenas de containers/VMs na SDN enche-a e o kernel comeca a
#     descartar vizinhos: trafego que funcionava para de funcionar por
#     intermitencia ("neighbour: arp_cache: neighbor table overflow").
#   ip_local_port_range — cada ligacao SAINTE por NAT consome uma porta
#     efemera. O default comeca em 32768 (~28k portas); um proxy ou um nó com
#     muito trafego de saida esgota-as e passa a dar EADDRNOTAVAIL.
#   pid_max / threads-max — muitos containers sao muitos processos; o default
#     de 32768 pids e baixo para um no denso.
#   file-max — fds a nivel do sistema (o limite por-processo trata-se abaixo).
#   tcp_max_syn_backlog + somaxconn — picos de ligacoes novas.
#   swappiness — trocar memoria de um container para disco degrada latencia de
#     forma dificil de diagnosticar; o kubelet ate exige swap desligado.
#
# LimitNOFILE/TasksMax vao para o `user@.service` porque em ROOTLESS os
# containers sao filhos dele: os limites do PAM/limits.conf de uma sessao SSH
# nao se aplicam ao que o systemd --user arranca.
if [ "$PRODUCTION" = 1 ]; then
  step kernel production "applying scale limits (conntrack/ARP/ports/fds/pids)..."
  $SUDO tee /etc/sysctl.d/99-delonix-production.conf >/dev/null <<'SYSCTL'
# Delonix Runtime — limites de ESCALA para um no de producao (install.sh --production).
# Reverter: rm este ficheiro e reiniciar (ou `sysctl --system`).
net.netfilter.nf_conntrack_max = 524288
net.ipv4.neigh.default.gc_thresh1 = 4096
net.ipv4.neigh.default.gc_thresh2 = 8192
net.ipv4.neigh.default.gc_thresh3 = 16384
net.ipv4.ip_local_port_range = 16384 65535
net.ipv4.tcp_max_syn_backlog = 8192
net.core.somaxconn = 8192
net.core.netdev_max_backlog = 16384
kernel.pid_max = 262144
kernel.threads-max = 1048576
fs.file-max = 2097152
vm.swappiness = 10
SYSCTL
  # `nf_conntrack_max` so existe depois de o modulo estar carregado, e o
  # `hashsize` NAO e um sysctl — e um parametro do modulo. Sem ele, subir o max
  # so alonga as cadeias do hash e a procura fica mais lenta em vez de escalar.
  $SUDO modprobe nf_conntrack 2>/dev/null || true
  printf 'options nf_conntrack hashsize=131072
'     | $SUDO tee /etc/modprobe.d/delonix-conntrack.conf >/dev/null
  printf 'nf_conntrack
' | $SUDO tee -a /etc/modules-load.d/delonix.conf >/dev/null
  if $SUDO sysctl -q -p /etc/sysctl.d/99-delonix-production.conf >/dev/null 2>&1; then
    stepok kernel production
  else
    warn "some production sysctls did not apply (module not loaded yet?) — they retry on next boot"
  fi

  # Limites do systemd --user: e sob ele que os containers rootless correm.
  step kernel user-limits "raising LimitNOFILE/TasksMax on user@.service..."
  $SUDO mkdir -p /etc/systemd/system/user@.service.d
  $SUDO tee /etc/systemd/system/user@.service.d/50-delonix-limits.conf >/dev/null <<'UNIT'
# Delonix Runtime — limites do systemd --user (install.sh --production).
# Em rootless os containers sao filhos do user@<uid>.service, por isso os
# limites de uma sessao PAM/SSH nao lhes chegam.
[Service]
LimitNOFILE=1048576
TasksMax=infinity
UNIT
  $SUDO systemctl daemon-reload 2>/dev/null || true
  stepok kernel user-limits
  warn "user@.service limits take effect on the NEXT login (or: systemctl restart user@$(id -u "$REAL_USER").service, which kills that user's running workloads)"
fi

# ------------------------------------------------ completion (bash/zsh/fish)
#
# Era só bash. A completion deste motor é DINÂMICA (`clap_complete`'s engine):
# o script registado apenas chama o binário de volta, por isso o mesmo mecanismo
# serve as três shells e sugere nomes de containers/imagens/volumes/VMs reais,
# lidos do estado em disco — não uma lista congelada na instalação.
#
# Cada shell é best-effort e independente: um host sem zsh não é motivo para
# falhar a instalação, e um directório em falta é a forma normal de dizer "esta
# shell não está cá".
if [ "$WITH_BINARY" = 1 ] && [ -x "$BIN_DIR/delonix" ]; then
  _comp_installed=""
  if [ -d /etc/bash_completion.d ]; then
    "$BIN_DIR/delonix" completion shell bash 2>/dev/null | $SUDO tee /etc/bash_completion.d/delonix >/dev/null \
      && _comp_installed="bash"
  fi
  for _zdir in /usr/share/zsh/site-functions /usr/local/share/zsh/site-functions; do
    if [ -d "$_zdir" ]; then
      # O ficheiro TEM de se chamar `_delonix`: o zsh procura a função de
      # completion pelo nome do ficheiro no `fpath`, e um nome diferente é
      # carregado por ninguém — falha silenciosa clássica desta integração.
      "$BIN_DIR/delonix" completion shell zsh 2>/dev/null | $SUDO tee "$_zdir/_delonix" >/dev/null \
        && _comp_installed="$_comp_installed zsh"
      break
    fi
  done
  for _fdir in /usr/share/fish/vendor_completions.d /usr/local/share/fish/vendor_completions.d; do
    if [ -d "$_fdir" ]; then
      "$BIN_DIR/delonix" completion shell fish 2>/dev/null | $SUDO tee "$_fdir/delonix.fish" >/dev/null \
        && _comp_installed="$_comp_installed fish"
      break
    fi
  done
  [ -n "$_comp_installed" ] && stepok binary "completion ($_comp_installed)"
  [ -z "$_comp_installed" ] && warn "no completion directory found — register it by hand: delonix completion shell bash >> ~/.bashrc"
fi

# --------------------------------------- realce de sintaxe do VMfile (editores)
#
# Um `VMfile` não tem extensão (como um Dockerfile), por isso nenhum editor o
# reconhece sozinho. Sem realce, o erro que o parser recusa — ele falha FECHADO
# numa instrução que não conhece — parece texto igual a todo o resto até ao
# momento do build.
#
# Os ficheiros saem do BINÁRIO (`delonix syntax`), não deste script: o uso
# documentado é `curl … | bash`, que não tem repositório de onde copiar, e uma
# gramática guardada noutro sítio afasta-se do parser que devia descrever.
#
# Cada editor é best-effort e independente, como as completions: uma directoria
# que não existe é a forma normal de dizer "este editor não está cá".
if [ "$WITH_BINARY" = 1 ] && [ -x "$BIN_DIR/delonix" ]; then
  _syn=""
  # vim/neovim: leem `syntax/` + `ftdetect/` da sua própria directoria. As duas
  # metades são precisas — o `ftdetect` é o que liga o filetype ao nome VMfile,
  # e sem ele o `syntax/` fica instalado sem nunca ser aplicado a nada.
  for _vdir in "$REAL_HOME/.vim" "$REAL_HOME/.config/nvim"; do
    if [ -d "$_vdir" ] && "$BIN_DIR/delonix" syntax vim --dir "$_vdir" >/dev/null 2>&1; then
      if [ "$(id -u)" = 0 ]; then
        chown -R "$REAL_USER" "$_vdir/syntax" "$_vdir/ftdetect" 2>/dev/null || true
      fi
      _syn="$_syn $(basename "$_vdir")"
    fi
  done
  # VS Code: uma extensão é uma directoria dentro de `extensions/`. Fica activa
  # na próxima janela.
  for _cdir in "$REAL_HOME/.vscode/extensions" "$REAL_HOME/.vscode-server/extensions"; do
    if [ -d "$_cdir" ] && "$BIN_DIR/delonix" syntax vscode --dir "$_cdir/delonix.vmfile-0.1.0" >/dev/null 2>&1; then
      if [ "$(id -u)" = 0 ]; then
        chown -R "$REAL_USER" "$_cdir/delonix.vmfile-0.1.0" 2>/dev/null || true
      fi
      _syn="$_syn vscode"
    fi
  done
  if [ -n "$_syn" ]; then
    stepok binary "VMfile syntax ($_syn)"
  else
    warn "no editor directory found for the VMfile syntax — install it by hand: delonix syntax vim --dir ~/.vim"
  fi
fi

# ------------------------------------------------------------------- manpages
#
# `man delonix-container-run` é como se lê um manual quando não se tem a CLI à
# frente. As páginas são GERADAS pelo próprio binário (`delonix man --dir`), por
# isso são sempre as deste binário e não as de uma versão anterior que ficou no
# sistema.
#
# `mandb` a seguir, senão o `man` não as encontra por nome até ao próximo cron
# diário — e "instalei e não funciona" é indistinguível de não ter instalado.
if [ "$WITH_BINARY" = 1 ] && [ -x "$BIN_DIR/delonix" ]; then
  case "$BIN_DIR" in
    "$REAL_HOME"/*) _mandir="$REAL_HOME/.local/share/man"; _mansudo="" ;;
    *)              _mandir="/usr/local/share/man";        _mansudo="$SUDO" ;;
  esac
  if $_mansudo mkdir -p "$_mandir/man1" 2>/dev/null \
     && "$BIN_DIR/delonix" man --dir "$TMP/man" >/dev/null 2>&1 \
     && $_mansudo cp "$TMP"/man/man1/*.1 "$_mandir/man1/" 2>/dev/null; then
    stepok binary "man pages -> $_mandir/man1"
    $_mansudo mandb -q >/dev/null 2>&1 || true
  else
    warn "could not install the man pages (generate them by hand: delonix man --dir ~/.local/share/man)"
  fi
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

# ------------------------------------------------ ACTIVAR a delegação de cgroup
# Escrever o drop-in é a diferença entre avisar e resolver. Um aviso deixa o
# utilizador com um problema de pesquisa cuja resposta está espalhada por
# metades: quase toda a documentação online cobre o `user@.service` e esquece
# que uma sessão de login é IRMÃ dele.
#
# `Delegate=cpu cpuset io memory pids` e NÃO `Delegate=yes`: medido neste host,
# o `yes` produziu apenas `memory pids`. Isso passa em todas as verificações que
# o motor faz e mata um nó `kindest/node` no arranque
# (`UserNS: cpu controller needs to be delegated`) — uma delegação parcial que
# se lê como completa é pior do que nenhuma.
DELEGATE_DROPIN=/etc/systemd/system/user@.service.d/50-delonix-delegate.conf
if [ "$WITH_DELEGATE" = 1 ] && [ "$USER_INSTALL" != 1 ] && [ -d /run/systemd/system ]; then
  # A distro pode já delegar o `cpu` por omissão — o Ubuntu 24.04 traz
  # `Delegate=pids memory cpu` no `user@.service`. Escrever um drop-in por cima
  # disso é mexer no /etc de toda a máquina para repetir o que já lá está, e
  # dá a impressão de ter resolvido um problema que o utilizador continua a ter
  # (o que costuma faltar é o `cpu` chegar ao slice de ONDE ele corre, não a
  # delegação em si). Perguntar é uma linha; a alternativa é uma alteração
  # global inútil.
  _u_deleg=$(systemctl cat user@.service 2>/dev/null | sed -n 's/^Delegate=//p' | tail -1)
  if [ -f "$DELEGATE_DROPIN" ]; then
    skip rootless delegation
  elif [ "$_u_deleg" = yes ] || case " $_u_deleg " in *" cpu "*) true ;; *) false ;; esac; then
    skip rootless delegation
    printf '[skip] %s\n' "user@.service already delegates: $_u_deleg (no drop-in needed)"
  elif [ -z "$SUDO" ] && [ "$(id -u)" != 0 ]; then
    warn "no sudo: skipping the cgroup delegation drop-in (resource limits will be inert)"
  else
    step rootless delegation "delegating cgroup controllers to user@.service..."
    $SUDO mkdir -p /etc/systemd/system/user@.service.d 2>/dev/null || true
    if $SUDO tee "$DELEGATE_DROPIN" >/dev/null <<'DELEGATE'
# Delonix Runtime — written by install.sh.
#
# Gives each user's systemd manager a delegated cgroup subtree, so rootless
# containers can actually carry --memory/--cpus/--pids-limit, and so a
# Kubernetes node can boot (its entrypoint refuses without `cpu`).
#
# The controllers are NAMED rather than `Delegate=yes`: on some hosts `yes`
# yields only `memory pids`, which passes every check and still kills a node.
#
# Revert: rm this file && systemctl daemon-reload
[Service]
Delegate=cpu cpuset io memory pids
DELEGATE
    then
      $SUDO systemctl daemon-reload 2>/dev/null || true
      stepok rootless delegation
      NEED_RELOGIN=1
    else
      warn "could not write $DELEGATE_DROPIN — resource limits will be inert"
    fi
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
if [ "$NEED_GPU_PROOF" = 1 ]; then
  check "CDI spec (--gpus all)"        sh -c 'ls /etc/cdi/*.yaml /etc/cdi/*.json /var/run/cdi/*.yaml /var/run/cdi/*.json >/dev/null 2>&1'
fi
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
# QUAIS controladores, não só "há delegação". A sonda acima responde "consigo
# criar um filho e mexer no subtree_control" — necessário, e cego ao que
# interessa: um host com `memory pids` delegados passa nela e não arranca um nó
# Kubernetes. Mesmo ponto cego que o `delonix system setup` teve até à v0.43.1.
CGROUP_HAVE=$(cat "/sys/fs/cgroup$(sed -n 's|^0::||p' /proc/self/cgroup 2>/dev/null)/cgroup.controllers" 2>/dev/null || true)
CGROUP_MISSING=""
for _c in cpu cpuset io memory pids; do
  case " $CGROUP_HAVE " in *" $_c "*) ;; *) CGROUP_MISSING="$CGROUP_MISSING $_c" ;; esac
done
if [ "$CGROUP_DELEGATED" = 1 ] && [ -z "$CGROUP_MISSING" ]; then
  stepok verify "cgroup delegation (resource limits apply)"
elif [ "$CGROUP_DELEGATED" = 1 ]; then
  printf '[verify] %s: %sPARTIAL%s (have:%s · missing:%s)\n' \
    "cgroup delegation" "$C_WARN" "$C_0" " ${CGROUP_HAVE:-none}" "$CGROUP_MISSING"
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

if [ "$CGROUP_DELEGATED" = 1 ] && [ -n "$CGROUP_MISSING" ]; then
  echo
  warn "delegation is PARTIAL:$CGROUP_MISSING not delegated"
  cat <<CGPART
    Container limits work, but \`delonix cluster create\` (kind mode) will not:
    a Kubernetes node's entrypoint refuses to boot without the \`cpu\` controller
    (\`UserNS: cpu controller needs to be delegated\`).

    The drop-in this installer writes fixes it — it takes effect on the NEXT
    login, because a running user@.service keeps the old setting:

        $DELEGATE_DROPIN

    Log out and back in, then check with:  delonix system setup
CGPART
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
if [ "$NEED_GPU_PROOF" = 1 ]; then
  echo
  msg "GPU configured — prove it end to end (this pulls a ~80MB image, which is why the installer does not do it for you):"
  cat <<'GPUPROOF'
    delonix container run --rm --gpus all ubuntu:24.04 nvidia-smi

    It must print the driver table. If it prints `nvidia-smi: not found`, the
    engine is older than the fix for CDI top-level containerEdits — the whole
    driver (nvidia-smi and every library) lives there, not in the per-device
    entries, and an engine that reads only the latter hands the container
    /dev/nvidia0 and nothing else.
GPUPROOF
fi

}  # fim do bloco de protecção contra download truncado (ver o topo do ficheiro)
