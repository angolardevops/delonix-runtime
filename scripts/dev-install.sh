#!/usr/bin/env bash
# dev-install.sh — install the binaries built from THIS source tree. Driven by the
# Makefile (`make install`, `make install-system`, `make uninstall`, `make apparmor`).
#
# It copies what cargo already built; it never compiles and never downloads. Installing
# a published release, and preparing a host to run containers and VMs, is
# `scripts/install.sh`.
#
#   dev-install.sh install   --from <dir> --prefix <dir> [--root <dir>] [--no-shell-rc] [--destdir <dir>]
#   dev-install.sh install   --system --from <dir> --prefix <dir> [--force] [--destdir <dir>]
#   dev-install.sh uninstall [--system] --prefix <dir> [--force] [--destdir <dir>]
#   dev-install.sh apparmor  --prefix <dir>
#
# The five binaries always travel together: `delonix` execs the servers that sit next
# to it and each one refuses a CLI from another build, so a partial install either
# fails to start or runs code nobody meant to test.
set -euo pipefail

BINARIES=(delonix delonix-cri delonix-mgmt delonix-mcp delonix-node-api)
RC_BEGIN='# >>> delonix dev env (make install) >>>'
RC_END='# <<< delonix dev env <<<'

die()  { printf 'error: %s\n' "$*" >&2; exit 1; }
note() { printf '%s\n' "$*"; }
warn() { printf 'warning: %s\n' "$*" >&2; }

ACTION=${1:-}; shift || true
FROM="" PREFIX="" ROOT="" DESTDIR="" SYSTEM=0 SHELL_RC=1 FORCE=0
while [ $# -gt 0 ]; do
  case "$1" in
    --from)        FROM=${2:?--from needs a directory}; shift ;;
    --prefix)      PREFIX=${2:?--prefix needs a directory}; shift ;;
    --root)        ROOT=${2-}; shift ;;
    --destdir)     DESTDIR=${2:?--destdir needs a directory}; shift ;;
    --system)      SYSTEM=1 ;;
    --no-shell-rc) SHELL_RC=0 ;;
    --force)       FORCE=1 ;;
    *) die "unknown argument: $1" ;;
  esac
  shift
done
[ -n "$PREFIX" ] || die "--prefix is required"

BINDIR="$DESTDIR$PREFIX/bin"
SHAREDIR="$DESTDIR$PREFIX/share"
CONFIG_HOME=${XDG_CONFIG_HOME:-$HOME/.config}
ENV_FILE="$CONFIG_HOME/delonix/env.sh"
FISH_FILE="$CONFIG_HOME/fish/conf.d/delonix-dev.fish"

# Writing below a system prefix needs root; a staged install (--destdir) or a prefix
# the caller owns does not.
SUDO=""
if [ "$SYSTEM" = 1 ] && [ -z "$DESTDIR" ] && [ "$(id -u)" != 0 ]; then
  command -v sudo >/dev/null 2>&1 || die "installing into $PREFIX needs root and sudo is not available"
  SUDO="sudo"
fi

# Engine processes of this user (or of anyone, for a system install) that are running
# right now. They keep the code they started with, so replacing the file under them
# leaves two builds running side by side until they exit.
running_engine() {
  local scope=(-u "$(id -u)")
  [ "$SYSTEM" = 1 ] && scope=()
  local b
  for b in "${BINARIES[@]}"; do pgrep "${scope[@]}" -x "$b" -a 2>/dev/null || true; done
}

apparmor_restricts() {
  [ "$(sysctl -n kernel.apparmor_restrict_unprivileged_userns 2>/dev/null || echo 0)" = 1 ]
}

apparmor_covers() { # $1 = absolute path of the delonix binary
  grep -rqsF -- "$1" /etc/apparmor.d 2>/dev/null
}

do_install() {
  [ -n "$FROM" ] || die "--from is required"
  local b
  for b in "${BINARIES[@]}"; do
    [ -x "$FROM/$b" ] || die "$FROM/$b is missing — run \`make build\` first"
  done

  local running; running=$(running_engine)
  if [ -n "$running" ] && [ -z "$DESTDIR" ]; then
    if [ "$SYSTEM" = 1 ] && [ "$FORCE" != 1 ]; then
      printf '%s\n' "$running" >&2
      die "engine processes are running on this machine. A system install changes what boot units, the kubelet's CRI endpoint and every later command execute while these keep the old code. Stop them, or repeat with FORCE=1 if that is what you want."
    fi
    warn "$(printf '%s\n' "$running" | wc -l) engine process(es) are running and keep the code they started with until they exit"
  fi

  $SUDO install -d "$BINDIR"
  for b in "${BINARIES[@]}"; do
    # install(1) unlinks the destination before writing, so replacing a binary that is
    # executing does not fail with ETXTBSY and does not rewrite the running image.
    $SUDO install -m 0755 "$FROM/$b" "$BINDIR/$b"
  done
  note "installed ${BINARIES[*]} -> $BINDIR"

  install_generated

  if [ "$SYSTEM" = 0 ] && [ -z "$DESTDIR" ]; then
    write_env
  fi

  note
  # The first three lines carry the version and the commit; the rest is the banner.
  local version
  if version=$("$BINDIR/delonix" --version 2>/dev/null); then
    printf '%s\n' "$version" | sed -n '1,3p'
  else
    warn "the installed binary did not run"
  fi
  if [ -z "$DESTDIR" ]; then
    local seen; seen=$(command -v delonix 2>/dev/null || true)
    if [ -n "$seen" ] && [ "$seen" != "$PREFIX/bin/delonix" ]; then
      warn "your shell resolves \`delonix\` to $seen, which comes before $PREFIX/bin on PATH — that one keeps running until PATH changes"
    fi
    if apparmor_restricts && ! apparmor_covers "$PREFIX/bin/delonix"; then
      if [ "$SYSTEM" = 1 ]; then
        warn "this host restricts unprivileged user namespaces and no AppArmor profile names $PREFIX/bin/delonix — rootless containers will fail with EPERM. Prepare the host with: bash scripts/install.sh --no-binary"
      else
        warn "this host restricts unprivileged user namespaces and no AppArmor profile names $PREFIX/bin/delonix — rootless containers will fail with EPERM. Run: make apparmor"
      fi
    fi
  fi
}

# Completion and man pages are generated by the binary itself, so they always describe
# the build that was just installed. Best-effort: a missing page never fails an install.
install_generated() {
  local cli="$BINDIR/delonix" tmp
  tmp=$(mktemp -d)
  # shellcheck disable=SC2064
  trap "rm -rf '$tmp'" RETURN
  local done_=""
  if "$cli" completion shell bash >"$tmp/bash" 2>/dev/null && [ -s "$tmp/bash" ]; then
    $SUDO install -D -m 0644 "$tmp/bash" "$SHAREDIR/bash-completion/completions/delonix" && done_="$done_ bash"
  fi
  if "$cli" completion shell zsh >"$tmp/zsh" 2>/dev/null && [ -s "$tmp/zsh" ]; then
    $SUDO install -D -m 0644 "$tmp/zsh" "$SHAREDIR/zsh/site-functions/_delonix" && done_="$done_ zsh"
  fi
  if "$cli" completion shell fish >"$tmp/fish" 2>/dev/null && [ -s "$tmp/fish" ]; then
    $SUDO install -D -m 0644 "$tmp/fish" "$SHAREDIR/fish/vendor_completions.d/delonix.fish" && done_="$done_ fish"
  fi
  [ -n "$done_" ] && note "completion:$done_ -> $SHAREDIR" || warn "could not generate shell completion"
  if "$cli" man --dir "$tmp/man" >/dev/null 2>&1 && [ -d "$tmp/man/man1" ]; then
    $SUDO install -d "$SHAREDIR/man/man1"
    $SUDO cp "$tmp"/man/man1/delonix*.1 "$SHAREDIR/man/man1/"
    note "man pages -> $SHAREDIR/man/man1"
  else
    warn "could not generate the man pages"
  fi
}

# The env file is what "loads" the install: DELONIX_BIN names the CLI the servers call
# back, DELONIX_ROOT the state root, and PATH gains the bin directory when it lacks it.
write_env() {
  local root=$ROOT
  [ -n "$root" ] || root="${XDG_DATA_HOME:-$HOME/.local/share}/delonix"
  case "$root" in "~/"*) root="$HOME/${root#"~/"}" ;; esac
  case "$root" in /*) ;; *) die "DELONIX_ROOT must be an absolute path, not '$root'" ;; esac

  install -d "$(dirname "$ENV_FILE")"
  {
    printf '# Written by `make install` from %s. Removed by `make uninstall`.\n' "$PWD"
    printf 'export DELONIX_BIN=%q\n' "$PREFIX/bin/delonix"
    printf 'export DELONIX_ROOT=%q\n' "$root"
    printf 'case ":$PATH:" in *":%s:"*) ;; *) export PATH=%q:"$PATH" ;; esac\n' "$PREFIX/bin" "$PREFIX/bin"
  } >"$ENV_FILE"
  note "env file: $ENV_FILE (DELONIX_BIN=$PREFIX/bin/delonix, DELONIX_ROOT=$root)"

  if [ "$SHELL_RC" != 1 ]; then
    note "not hooked into your shell (SHELL_RC=0). Load it with: . $ENV_FILE"
    return
  fi
  local rc hooked=""
  for rc in "$HOME/.bashrc" "$HOME/.zshrc"; do
    [ -f "$rc" ] || continue
    if ! grep -qsF "$RC_BEGIN" "$rc"; then
      printf '\n%s\n[ -r %q ] && . %q\n%s\n' "$RC_BEGIN" "$ENV_FILE" "$ENV_FILE" "$RC_END" >>"$rc"
    fi
    hooked="$hooked $rc"
  done
  if [ -d "$CONFIG_HOME/fish" ]; then
    install -d "$(dirname "$FISH_FILE")"
    {
      printf '# Written by `make install`. Removed by `make uninstall`.\n'
      printf 'set -gx DELONIX_BIN %q\n' "$PREFIX/bin/delonix"
      printf 'set -gx DELONIX_ROOT %q\n' "$root"
      printf 'fish_add_path %q\n' "$PREFIX/bin"
    } >"$FISH_FILE"
    hooked="$hooked $FISH_FILE"
  fi
  if [ -n "$hooked" ]; then
    note "loaded by:$hooked"
    note "new shells pick it up; for this one: . $ENV_FILE"
  else
    note "no ~/.bashrc or ~/.zshrc found. Load it with: . $ENV_FILE"
  fi
  note "note: scripts/cli-tree.sh and scripts/docs_cli_gate.py inspect \$DELONIX_BIN when it is set — unset it (or point it at target/release/delonix) before running them against the tree"
}

do_uninstall() {
  if [ "$SYSTEM" = 1 ] && [ -z "$DESTDIR" ] && [ "$FORCE" != 1 ]; then
    local running; running=$(running_engine)
    if [ -n "$running" ]; then
      printf '%s\n' "$running" >&2
      die "engine processes are running; removing the system binaries leaves them without a CLI to call back. Stop them, or repeat with FORCE=1."
    fi
  fi
  local b
  for b in "${BINARIES[@]}"; do $SUDO rm -f "$BINDIR/$b"; done
  $SUDO rm -f "$SHAREDIR/bash-completion/completions/delonix" \
    "$SHAREDIR/zsh/site-functions/_delonix" \
    "$SHAREDIR/fish/vendor_completions.d/delonix.fish"
  $SUDO sh -c 'rm -f "$1"/man/man1/delonix*.1' sh "$SHAREDIR"
  note "removed the binaries, completion and man pages from $DESTDIR$PREFIX"

  if [ "$SYSTEM" = 0 ] && [ -z "$DESTDIR" ]; then
    local rc
    for rc in "$HOME/.bashrc" "$HOME/.zshrc"; do
      [ -f "$rc" ] && grep -qsF "$RC_BEGIN" "$rc" || continue
      # Delete the marked block and the blank line written in front of it.
      sed -i -e "/^$RC_BEGIN\$/,/^$RC_END\$/d" "$rc"
      sed -i -e '${/^$/d}' "$rc"
      note "removed the hook from $rc"
    done
    rm -f "$ENV_FILE" "$FISH_FILE"
    note "removed $ENV_FILE"
    note "the state root was left untouched; DELONIX_ROOT/DELONIX_BIN stay set in shells already open"
  fi
}

# A second profile with its own name: `scripts/install.sh` owns /etc/apparmor.d/delonix
# and rewrites it, so reusing that file would take the profile away from an installed
# release.
do_apparmor() {
  local target="$PREFIX/bin/delonix"
  if ! apparmor_restricts; then
    note "this host does not restrict unprivileged user namespaces — no profile is needed"
    return
  fi
  if apparmor_covers "$target"; then
    note "an AppArmor profile already names $target"
    return
  fi
  command -v apparmor_parser >/dev/null 2>&1 || die "apparmor_parser is missing while the userns restriction is active"
  [ "$(id -u)" = 0 ] || SUDO="sudo"
  printf 'abi <abi/4.0>,\ninclude <tunables/global>\nprofile delonix-dev %s flags=(unconfined) {\n  userns,\n}\n' "$target" \
    | $SUDO tee /etc/apparmor.d/delonix-dev >/dev/null
  $SUDO apparmor_parser -r /etc/apparmor.d/delonix-dev
  note "AppArmor profile delonix-dev loaded for $target (remove with: sudo apparmor_parser -R /etc/apparmor.d/delonix-dev && sudo rm /etc/apparmor.d/delonix-dev)"
}

case "$ACTION" in
  install)   do_install ;;
  uninstall) do_uninstall ;;
  apparmor)  do_apparmor ;;
  *) die "usage: dev-install.sh install|uninstall|apparmor --prefix <dir> [options]" ;;
esac
