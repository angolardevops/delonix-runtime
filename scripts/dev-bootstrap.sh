#!/usr/bin/env bash
# dev-bootstrap.sh — prepare a machine to BUILD delonix-runtime. Driven by the Makefile
# (`make bootstrap`, `make doctor`).
#
#   dev-bootstrap.sh              install what is missing (asks sudo for distro packages)
#   dev-bootstrap.sh --check      report only; exit 1 when something required is missing
#
#   --no-packages       never call the distro package manager
#   --no-cargo-tools    do not `cargo install` sccache / cargo-deny
#   --with-mold         also install the mold linker (only used with `make LINKER=mold`)
#
# What a build of this workspace needs, and why:
#   Rust, pinned          rust-toolchain.toml names the channel; rustfmt and clippy are gates.
#   a C and C++ compiler  `ring` and `zstd-sys` compile C through the `cc` crate.
#   clang + lld           the C/C++ side of the LLVM toolchain; lld is also the linker rustc
#                         needs on targets where it is not the default (aarch64).
#   protoc                the CRI and node-contract build scripts (prost/tonic).
#   pkg-config, make, git, curl, python3 >= 3.11 (the script gates use tomllib).
#   cargo-nextest         `make test` runs the suite in about a fifth of the time.
#   sccache               a shared compile cache for Rust and C/C++ objects: a second
#                         worktree does not pay for the whole build again.
#
# This script is about the BUILD machine. Preparing a host to run containers and VMs
# (slirp4netns, subuid, AppArmor, kernel tuning) is `scripts/install.sh --no-binary`.
set -euo pipefail

CHECK=0 WITH_PACKAGES=1 WITH_CARGO_TOOLS=1 WITH_MOLD=0
for arg in "$@"; do
  case "$arg" in
    --check)          CHECK=1 ;;
    --no-packages)    WITH_PACKAGES=0 ;;
    --no-cargo-tools) WITH_CARGO_TOOLS=0 ;;
    --with-mold)      WITH_MOLD=1 ;;
    -h|--help)        sed -n '2,24p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) printf 'error: unknown argument: %s\n' "$arg" >&2; exit 2 ;;
  esac
done

ROOT_DIR=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT_DIR"
# rustup installs into ~/.cargo/bin, which a fresh shell may not have on PATH yet.
export PATH="${CARGO_HOME:-$HOME/.cargo}/bin:$PATH"

if [ -t 1 ]; then C_OK=$'\033[32m' C_BAD=$'\033[31m' C_DIM=$'\033[2m' C_0=$'\033[0m'; else C_OK="" C_BAD="" C_DIM="" C_0=""; fi
MISSING_REQUIRED=0
has() { command -v "$1" >/dev/null 2>&1; }
row() { # status, name, detail
  case "$1" in
    ok)   printf '  %sok%s       %-18s %s\n' "$C_OK" "$C_0" "$2" "$3" ;;
    miss) printf '  %sMISSING%s  %-18s %s\n' "$C_BAD" "$C_0" "$2" "$3"; MISSING_REQUIRED=1 ;;
    opt)  printf '  %s-%s        %-18s %s\n' "$C_DIM" "$C_0" "$2" "$3" ;;
  esac
}

CHANNEL=$(sed -n 's/^channel *= *"\(.*\)"/\1/p' rust-toolchain.toml)
[ -n "$CHANNEL" ] || { echo "error: could not read the channel from rust-toolchain.toml" >&2; exit 1; }

# ------------------------------------------------------------ distro packages
PKG=""
if [ -r /etc/os-release ]; then
  # shellcheck disable=SC1091
  ids=" $(. /etc/os-release; echo "${ID:-} ${ID_LIKE:-}") "
  case "$ids" in
    *" debian "*|*" ubuntu "*)                   PKG=apt ;;
    *" fedora "*|*" rhel "*|*" centos "*)        PKG=dnf ;;
    *" suse "*|*" opensuse "*|*" sles "*)        PKG=zypper ;;
    *" arch "*)                                  PKG=pacman ;;
  esac
fi

# The distro package that provides a command. Empty = not packaged under a known name.
pkg_for() { # $1 = command
  case "$PKG:$1" in
    apt:cc|apt:c++|apt:make)   echo build-essential ;;
    dnf:cc)                    echo gcc ;;
    dnf:c++)                   echo gcc-c++ ;;
    zypper:cc)                 echo gcc ;;
    zypper:c++)                echo gcc-c++ ;;
    pacman:cc|pacman:c++|pacman:make) echo base-devel ;;
    *:make|*:clang|*:git|*:curl|*:mold) echo "$1" ;;
    *:ld.lld)                  echo lld ;;
    apt:pkg-config|zypper:pkg-config) echo pkg-config ;;
    dnf:pkg-config)            echo pkgconf-pkg-config ;;
    pacman:pkg-config)         echo pkgconf ;;
    apt:protoc|dnf:protoc)     echo protobuf-compiler ;;
    zypper:protoc)             echo protobuf-devel ;;
    pacman:protoc)             echo protobuf ;;
    pacman:python3)            echo python ;;
    *:python3)                 echo python3 ;;
  esac
}

REQUIRED_CMDS=(git curl make cc c++ pkg-config protoc python3)
RECOMMENDED_CMDS=(clang ld.lld)
[ "$WITH_MOLD" = 1 ] && RECOMMENDED_CMDS+=(mold)

install_packages() {
  local want=() c p
  for c in "${REQUIRED_CMDS[@]}" "${RECOMMENDED_CMDS[@]}"; do
    has "$c" && continue
    p=$(pkg_for "$c")
    [ -n "$p" ] && want+=("$p")
  done
  [ ${#want[@]} -gt 0 ] || return 0
  if [ -z "$PKG" ]; then
    echo "warning: unknown distro — install these yourself: ${want[*]}" >&2
    return 0
  fi
  # De-duplicate (cc, c++ and make all map to build-essential on apt).
  mapfile -t want < <(printf '%s\n' "${want[@]}" | sort -u)
  local sudo=""
  if [ "$(id -u)" != 0 ]; then
    has sudo || { echo "warning: no sudo — install these yourself: ${want[*]}" >&2; return 0; }
    sudo="sudo"
  fi
  echo "installing distro packages ($PKG): ${want[*]}"
  case "$PKG" in
    apt)    $sudo apt-get update -qq && $sudo apt-get install -y --no-install-recommends "${want[@]}" ;;
    dnf)    $sudo dnf install -y "${want[@]}" ;;
    zypper) $sudo zypper --non-interactive install "${want[@]}" ;;
    pacman) $sudo pacman -S --noconfirm --needed "${want[@]}" ;;
  esac
}

# ------------------------------------------------------------------ toolchain
install_rust() {
  if ! has rustup; then
    echo "installing rustup (no default toolchain; the pinned one follows)"
    curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs \
      | sh -s -- -y --no-modify-path --profile minimal --default-toolchain none
  fi
  if ! rustup toolchain list 2>/dev/null | grep -q "^$CHANNEL"; then
    echo "installing the pinned toolchain $CHANNEL"
  fi
  # Idempotent: a toolchain that is already complete is left as it is.
  rustup toolchain install "$CHANNEL" --profile minimal --component rustfmt --component clippy >/dev/null
}

install_cargo_tools() {
  local tool
  for tool in sccache cargo-deny cargo-nextest; do
    has "$tool" && continue
    echo "installing $tool with cargo (compiles from source; a few minutes)"
    # Lowest priority and a bounded job count: bootstrapping must not be the build
    # that exhausts the machine it is preparing.
    nice -n 19 cargo install --locked --jobs "$(suggested_jobs)" "$tool" \
      || echo "warning: could not install $tool — the build works without it" >&2
  done
}

suggested_jobs() {
  local cores avail j
  cores=$(nproc 2>/dev/null || echo 2)
  avail=$(awk '/^MemAvailable:/{printf "%d", $2/1048576}' /proc/meminfo 2>/dev/null || echo 4)
  j=$(( (avail - 2) / 3 ))
  [ "$j" -lt 1 ] && j=1
  [ "$j" -gt "$cores" ] && j=$cores
  echo "$j"
}

if [ "$CHECK" = 0 ]; then
  [ "$WITH_PACKAGES" = 1 ] && install_packages
  install_rust
  [ "$WITH_CARGO_TOOLS" = 1 ] && install_cargo_tools
  echo
fi

# --------------------------------------------------------------------- report
ver() { "$@" 2>/dev/null | head -1 || true; }

echo "build environment for delonix-runtime ($ROOT_DIR)"
echo "required"
for c in "${REQUIRED_CMDS[@]}"; do
  if has "$c"; then row ok "$c" "$(command -v "$c")"; else row miss "$c" "package: $(pkg_for "$c")"; fi
done
if has python3 && ! python3 -c 'import sys; sys.exit(sys.version_info < (3, 11))'; then
  row miss "python3 >= 3.11" "found $(ver python3 --version) — the script gates use tomllib"
fi
if has rustup && rustup toolchain list 2>/dev/null | grep -q "^$CHANNEL"; then
  row ok "rust $CHANNEL" "$(ver rustc "+$CHANNEL" --version)"
  for comp in rustfmt clippy; do
    if rustup component list --toolchain "$CHANNEL" --installed 2>/dev/null | grep -q "^$comp"; then
      row ok "$comp" "component of $CHANNEL"
    else
      row miss "$comp" "rustup component add $comp --toolchain $CHANNEL"
    fi
  done
else
  row miss "rust $CHANNEL" "pinned in rust-toolchain.toml — run: make bootstrap"
fi

echo "recommended"
for c in clang ld.lld sccache cargo-deny cargo-nextest; do
  if has "$c"; then row ok "$c" "$(command -v "$c")"; else row opt "$c" "not installed"; fi
done
for c in nice ionice; do
  if has "$c"; then row ok "$c" "builds run at low CPU/disk priority"; else row opt "$c" "not installed — builds run at normal priority"; fi
done

# rustc ships its own LLVM. Cross-language LTO (Rust and C in one optimisation
# unit) needs the C compiler to be the same major; otherwise it is not available.
# Plain builds are unaffected, so this is informational, never a failure.
if has rustc && has clang; then
  rl=$(rustc "+$CHANNEL" -vV 2>/dev/null | sed -n 's/^LLVM version: \([0-9]*\).*/\1/p')
  cl=$(clang --version 2>/dev/null | sed -n 's/.*clang version \([0-9]*\).*/\1/p')
  if [ -n "$rl" ] && [ -n "$cl" ]; then
    if [ "$rl" = "$cl" ]; then row ok "llvm match" "rustc and clang are both LLVM $rl"
    else row opt "llvm match" "rustc uses LLVM $rl, clang is $cl: no cross-language LTO (plain builds unaffected)"; fi
  fi
fi

echo "optional (specific gates only)"
for pair in "mold:make LINKER=mold" "buf:scripts/contract_gate.py" "protoc-gen-openapi:scripts/contract_gate.py" \
            "cargo-llvm-cov:make coverage" "groff:man page check in the docs job"; do
  c=${pair%%:*}
  if has "$c"; then row ok "$c" "$(command -v "$c")"; else row opt "$c" "needed for: ${pair#*:}"; fi
done

if python3 -c 'import markdown' 2>/dev/null; then row ok "python markdown" "make gates (handbook site check)"; else row opt "python markdown" "needed for: make gates (pip install markdown)"; fi

echo "machine"
printf '  %-18s %s\n' "cores" "$(nproc 2>/dev/null || echo '?')"
printf '  %-18s %s GiB available of %s GiB\n' "memory" \
  "$(awk '/^MemAvailable:/{printf "%d", $2/1048576}' /proc/meminfo)" \
  "$(awk '/^MemTotal:/{printf "%d", $2/1048576}' /proc/meminfo)"
printf '  %-18s %s (3 GiB per rustc/linker, 2 GiB kept free)\n' "suggested jobs" "$(suggested_jobs)"
case "$(uname -m)" in
  x86_64) printf '  %-18s %s\n' "linker" "LLD, the default of the pinned toolchain on x86_64" ;;
  *)      printf '  %-18s %s\n' "linker" "system default on $(uname -m); \`make LINKER=lld\` switches (one full rebuild)" ;;
esac

if [ "$MISSING_REQUIRED" = 1 ]; then
  echo
  if [ "$CHECK" = 1 ]; then echo "something required is missing — run: make bootstrap"; else echo "something required is still missing (see above)"; fi
  exit 1
fi
