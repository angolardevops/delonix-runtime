#!/bin/bash
# Acceptance matrix of the `delonix init` templates against the REAL binary
# (ADR-0061). Generation-level only — it runs no package manager and needs no
# network: every template renders, leaves no placeholder, writes JSON/YAML/TOML
# that parse and a manifest that validates; a bad name, a bad or out-of-range
# -v and a symlinked destination are refused and write nothing; a second run
# preserves edits; detection reads manifests; adoption keeps every existing
# file and writes CI only where the project can run it.
#
#   scripts/init-accept.sh <delonix-binary> <empty-work-dir>
#
# Exit 0 when every check passes. What a generated project's own tests prove
# (lint, typecheck, tests, build, smoke) is that project's `make check` /
# `pnpm check`, run from the directory this script leaves in <work-dir>/gen-*.
set -u
[ $# = 2 ] || { echo "usage: $0 <delonix-binary> <work-dir>" >&2; exit 2; }
B="$1"; W="$2"; mkdir -p "$W"; cd "$W" || exit 2
pass=0; fail=0
ok()  { pass=$((pass+1)); echo "PASS  $*"; }
bad() { fail=$((fail+1)); echo "FAIL  $*"; }
check() { local name="$1"; shift; if "$@" >/dev/null 2>&1; then ok "$name"; else bad "$name"; fi; }
refuse() { # name, dir-that-must-not-exist, cmd...
  local name="$1" dir="$2"; shift 2
  if "$@" </dev/null >/dev/null 2>&1; then bad "$name (accepted)"; elif [ -e "$dir" ]; then bad "$name (refused but wrote $dir)"; else ok "$name"; fi; }

TEMPLATES=$("$B" stack init -t list 2>/dev/null | sed 's/.*: //; s/,//g')
echo "templates: $TEMPLATES"

parse_all() { python3 - "$1" <<'EOF'
import json, sys, os, tomllib
try:
    import yaml
except ImportError:
    yaml = None
root = sys.argv[1]; errs = []
for d, dirs, files in os.walk(root):
    dirs[:] = [x for x in dirs if x not in ('node_modules', '.venv', 'vendor', '.git')]
    for f in files:
        p = os.path.join(d, f)
        try:
            if f.endswith('.json'): json.load(open(p))
            elif f.endswith('.toml'): tomllib.load(open(p, 'rb'))
            elif f.endswith(('.yaml', '.yml')) and yaml: list(yaml.safe_load_all(open(p)))
        except Exception as e:
            errs.append(f"{p}: {e}")
print("\n".join(errs)); sys.exit(1 if errs else 0)
EOF
}

for t in $TEMPLATES; do
  d="gen-$t"; rm -rf "$d"
  if "$B" init -t "$t" --name "my-$t-svc" "$d" </dev/null >"$d.out" 2>&1; then ok "$t: generates"; else bad "$t: generates ($(tail -1 "$d.out"))"; continue; fi
  left=$(grep -rIl '__NAME__\|__MODULE__\|__PORT__\|__TEMPLATE_VERSION__' "$d" | head -3)
  [ -z "$left" ] && ok "$t: no placeholder left" || bad "$t: placeholder left in $left"
  out=$(parse_all "$d"); [ $? = 0 ] && ok "$t: every JSON/YAML/TOML parses" || bad "$t: unparsable: $out"
  check "$t: manifest validates" "$B" manifest validate -f "$d/delonix-manifest.yaml"
  # second run: an edit survives, nothing is re-created
  echo "# edited by the user" >> "$d/Delonixfile"; h=$(sha256sum "$d/Delonixfile" | cut -d' ' -f1); n=$(find "$d" -type f | wc -l)
  "$B" init -t "$t" --name "my-$t-svc" "$d" </dev/null >/dev/null 2>&1
  [ "$h" = "$(sha256sum "$d/Delonixfile" | cut -d' ' -f1)" ] && [ "$n" = "$(find "$d" -type f | wc -l)" ] \
    && ok "$t: a second run preserves edits and adds nothing" || bad "$t: second run changed the project"
done

# A directory is named for people: the project name is derived from it and
# said. An explicit --name is never rewritten (next check).
derived() { # name, dir, expected-project-name, cmd...
  local name="$1" dir="$2" want="$3"; shift 3; local out
  out=$("$@" </dev/null 2>&1) && echo "$out" | grep -q "using '$want'" && grep -q "name: $want\$" "$dir/delonix-manifest.yaml" \
    && ok "$name" || bad "$name ($(echo "$out" | tail -1))"; }
derived 'a directory with a space and a quote gives a derived name' 'My App"x' my-app-x "$B" init -t go 'My App"x'
derived 'an uppercase directory gives a lowercase name' 'Weird' weird "$B" init -t node Weird
refuse 'name via --name is checked too' 'nm' "$B" init -t go --name 'Bad_Name' nm
refuse '-v injection is refused' 'inj' "$B" init -t django -v '5.2.*", "evil==1' inj
refuse '-v outside the range is refused (next 14)' 'nx14' "$B" init -t nextjs -v 14.2.0 nx14
refuse '-v outside the range is refused (go 1.23)' 'go123' "$B" init -t go -v 1.23 go123
refuse 'unknown template is refused' 'nope' "$B" init -t nope nope
mkdir -p outside sl && echo '{"dependencies":{"fastify":"5"}}' > sl/package.json && ln -sfn ../outside/pwned sl/Delonixfile
before=$(ls -A sl | wc -l); "$B" init sl </dev/null >/dev/null 2>&1; rc=$?
[ $rc != 0 ] && [ "$before" = "$(ls -A sl | wc -l)" ] && [ ! -e outside/pwned ] && ok 'a symlinked destination refuses and writes nothing' || bad "symlink: rc=$rc files $(ls -A sl | wc -l)"

# detection
det() { # name, expected-substring, setup-commands (run in a fresh dir)
  local name="$1" want="$2"; shift 2; local d="det-$RANDOM"; mkdir -p "$d"; ( cd "$d" && eval "$*" )
  local out; out=$( cd "$d" && "$B" init --name det-x </dev/null 2>&1 )
  echo "$out" | grep -q -- "$want" && ok "detect: $name" || bad "detect: $name (got: $(echo "$out" | grep -E 'detected|found|using' | head -2 | tr '\n' ' '))"; }
det 'express package.json is unknown' 'no template exists' "echo '{\"dependencies\":{\"express\":\"4\"}}' > package.json"
det 'fastify package.json is node' 'template node' "echo '{\"dependencies\":{\"fastify\":\"5\"}}' > package.json"
det 'next wins over fastify' 'template nextjs' "echo '{\"dependencies\":{\"next\":\"16\",\"fastify\":\"5\"}}' > package.json"
det 'symfony composer.json is unknown' 'no template exists' "echo '{\"require\":{\"symfony/framework-bundle\":\"7\"}}' > composer.json"
det 'laravel composer.json is laravel' 'template laravel' "echo '{\"require\":{\"laravel/framework\":\"^13\"}}' > composer.json"
det 'flask pyproject is unknown' 'no template exists' "printf '[project]\nname=\"x\"\ndependencies=[\"flask\"]\n' > pyproject.toml"
det 'django requirements.txt is django' 'template django' "echo 'Django==5.2.1' > requirements.txt"
det 'fastapi pyproject is fastapi' 'template fastapi' "printf '[project]\nname=\"x\"\ndependencies=[\"fastapi>=0.1\"]\n' > pyproject.toml"
det 'compose is already served' 'delonix compose up' "echo 'services: {}' > compose.yaml"
d=det-explicit; mkdir -p $d; echo 'services: {}' > $d/compose.yaml
out=$( cd $d && "$B" init -t go --name ex-go </dev/null 2>&1 )
echo "$out" | grep -q 'two descriptions' && [ -f $d/Delonixfile ] && ok 'explicit -t wins over compose, with a warning' || bad 'explicit -t over compose'

# adoption: an npm project with its own CI keeps every hash and gets no pnpm CI
a=adopt-npm; rm -rf $a; mkdir -p $a/.github/workflows $a/src
echo '{"name":"real","dependencies":{"fastify":"5"},"scripts":{"test":"jest"}}' > $a/package.json
echo '{}' > $a/package-lock.json; echo '# Real' > $a/README.md; echo 'name: own' > $a/.github/workflows/test.yml; echo 'x' > $a/src/server.js
h1=$(cd $a && find . -type f | sort | xargs sha256sum | sha256sum)
out=$( cd $a && "$B" init --name real </dev/null 2>&1 )
h2=$(cd $a && sha256sum package.json package-lock.json README.md .github/workflows/test.yml src/server.js | sha256sum)
h1b=$(cd $a && sha256sum package.json package-lock.json README.md .github/workflows/test.yml src/server.js | sha256sum)
echo "$out" | grep -q 'CI files not written' && [ ! -e $a/.github/workflows/ci.yml ] && [ ! -e $a/.gitlab-ci.yml ] && [ ! -e $a/src/app.ts ] && [ -f $a/Delonixfile ] \
  && ok 'adopt: own CI and npm lock → no template CI, no demo code, glue written' || bad "adopt npm: $(ls -A $a | tr '\n' ' ')"
[ "$(cd $a && cat README.md)" = '# Real' ] && ok 'adopt: README untouched' || bad 'adopt: README changed'
# adoption: a pnpm project without CI gets the CI
p=adopt-pnpm; rm -rf $p; mkdir -p $p; echo '{"name":"real2","dependencies":{"fastify":"5"}}' > $p/package.json; touch $p/pnpm-lock.yaml
( cd $p && "$B" init --name real2 </dev/null >/dev/null 2>&1 ); [ -f $p/.github/workflows/ci.yml ] && ok 'adopt: pnpm project without CI gets the CI' || bad 'adopt pnpm: no CI written'

echo "== $pass passed, $fail failed"
[ $fail = 0 ]
