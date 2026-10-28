#!/usr/bin/env bash
# Rebrand of the product/repo identity: angolardevops/delonix-runtime -> ngolacloud/delonix.
#
# Scope (approved): PRODUCT and REPO identity only. This script DOES NOT rename
# internal Cargo crate/bin packages (`delonix-runtime-bin`, `delonix-runtime-core`,
# ...), and it NEVER touches the persisted operational contracts:
#   state dirs (/var/lib/delonix, /etc/delonix, ~/.local/share/delonix),
#   sockets (/run/delonix-*.sock), systemd service UNIT NAMES, env DELONIX_*,
#   schema groups *.delonix.io. Those stayed stable across the 5.x->0.x re-base.
#
# Two sections:
#   safe          — config/prose that does NOT bake into the binary nor feed the
#                   docs generators / freshness gate. Safe to apply and commit on
#                   its own; gates stay green.
#   build-coupled — Rust URL constants (main.rs/codes.rs/...), docs/gen.py and
#                   scripts/dev_docs_site.py constants, docs/dev/**/*.md, and the
#                   .proto go_package. These change the BINARY and the GENERATED
#                   docs/Go, so they MUST be followed by a rebuild + regen + buf
#                   in the same change, or the docs freshness gate and buf gate
#                   go red. Run this with the Sprint 6 release build.
#
# Historical records are left untouched on purpose (they document past versions
# and the migration itself): docs/releases/*, docs/RELEASES.md,
# docs/SECURITY-RELEASES.md, reports/*, CHANGELOG.md, VERSIONING.md, and every
# generated docs/**/*.html (regenerated from source, never sed'd here).
#
# Usage: scripts/rebrand.sh {safe|build-coupled|all}
set -euo pipefail
cd "$(dirname "$0")/.."

sub() {  # sub OLD NEW FILE...  (in-place, only the listed files)
  local old="$1" new="$2"; shift 2
  for f in "$@"; do [ -f "$f" ] && sed -i "s#${old}#${new}#g" "$f"; done
}

URL_OLD='github.com/angolardevops/delonix-runtime'; URL_NEW='github.com/ngolacloud/delonix'
PG_OLD='angolardevops.github.io/delonix-runtime';   PG_NEW='ngolacloud.github.io/delonix'
GHCR_OLD='ghcr.io/angolardevops/delonix-runtime';   GHCR_NEW='ghcr.io/ngolacloud/delonix'

rebrand_safe() {
  local files=(
    Cargo.toml Makefile Delonixfile
    dist/delonix-cri.service dist/delonix-node-api.service dist/delonix-node-api.socket
    dist/delonix-storage-helper@.service dist/delonix-storage-helper.socket
    .github/ISSUE_TEMPLATE/config.yml
    scripts/sbom.py
    README.rst CONTRIBUTING.md SECURITY.md CODE_OF_CONDUCT.md
  )
  sub "$GHCR_OLD" "$GHCR_NEW" "${files[@]}"   # before URL_OLD (ghcr path is a superstring)
  sub "$URL_OLD"  "$URL_NEW"  "${files[@]}"
  sub "$PG_OLD"   "$PG_NEW"   "${files[@]}"
  sub 'Organization: angolardevops' 'Organization: ngolacloud' scripts/sbom.py
  sub 'title="delonix-runtime"' 'title="delonix"' Delonixfile
  echo "safe: applied. The CLI binary is already named 'delonix'; crate/bin"
  echo "      package names are intentionally unchanged (deferred)."
}

rebrand_build_coupled() {
  local rs=(
    bins/delonix-runtime-bin/src/main.rs
    bins/delonix-runtime-bin/src/cmd/providers_config.rs
    bins/delonix-runtime-bin/src/cmd/vmimage.rs
    bins/delonix-runtime-bin/src/cmd/container.rs
    bins/delonix-runtime-bin/src/cmd/scaffold.rs
    crates/foundation/delonix-model/src/codes.rs
    crates/adapters/delonix-scanner/src/lib.rs
  )
  local docsrc=(docs/gen.py scripts/dev_docs_site.py scripts/install.sh scripts/test_install_release_link.py)
  mapfile -t devmd < <(find docs/dev -name '*.md')
  mapfile -t protos < <(find proto -name '*.proto')
  sub "$GHCR_OLD" "$GHCR_NEW" "${rs[@]}" "${docsrc[@]}" "${devmd[@]}" "${protos[@]}"
  sub "$URL_OLD"  "$URL_NEW"  "${rs[@]}" "${docsrc[@]}" "${devmd[@]}" "${protos[@]}"
  sub "$PG_OLD"   "$PG_NEW"   "${rs[@]}" "${docsrc[@]}" "${devmd[@]}" "${protos[@]}"
  cat <<'EOF'
build-coupled: source edited. NOW run, or the docs/buf gates go red:
  cargo build --release -p delonix-runtime-bin --bin delonix   # + the 4 server bins for a release
  python3 docs/gen.py                 # regenerate docs/*.html from the rebuilt --help
  python3 scripts/dev_docs_site.py    # regenerate docs/handbook/ from docs/dev/*.md
  buf generate                        # regenerate gen/go from the new go_package
  python3 scripts/version_gate.py && python3 scripts/adr_status_gate.py
  python3 scripts/dev_docs_site.py --check && git diff --quiet -- docs/ && echo "docs fresh"
EOF
}

case "${1:-}" in
  safe) rebrand_safe ;;
  build-coupled) rebrand_build_coupled ;;
  all) rebrand_safe; rebrand_build_coupled ;;
  *) echo "usage: $0 {safe|build-coupled|all}" >&2; exit 2 ;;
esac
