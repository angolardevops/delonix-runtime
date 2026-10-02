#!/bin/bash
# Runs the CI that `delonix init -t <template>` generates, for real, in a clean
# rootless container — no hosted runner involved.
#
#   scripts/init-ci.sh <delonix-binary> <work-dir> [template ...]
#
# For each application template:
#   1. generate the project with the given binary;
#   2. start a container from the image the project's .gitlab-ci.yml names,
#      with the project bind-mounted, and inside it:
#        a. create the lock the CI requires, with the command the README gives
#           (a generated project has none: only go.sum ships);
#        b. run the GitLab job exactly as written: variables, before_script,
#           script;
#        c. run every `run:` step of .github/workflows/ci.yml, in order.
#
# What this does NOT prove: the `uses:` actions of the GitHub workflow
# (checkout, setup-*). They are replaced here by the GitLab job's image and
# before_script, which install the same toolchain. Everything the workflows
# execute themselves is executed.
#
# Uses the caller's DELONIX_ROOT / DELONIX_NET_RUNTIME_DIR; set both to keep
# the run away from the machine's real state. Needs network (registries and
# package indexes) and python3 with PyYAML.
set -u
B="${1:?usage: init-ci.sh <delonix-binary> <work-dir> [template ...]}"
W="${2:?usage: init-ci.sh <delonix-binary> <work-dir> [template ...]}"
shift 2
mkdir -p "$W"
W=$(cd "$W" && pwd)
TEMPLATES="${*:-go node nestjs nextjs fastapi django laravel}"
pass=0; fail=0

# The command that creates the lock, per template (a generated project has
# none; the README tells the developer to create and commit it). "pre" runs
# before the job, with the one tool it needs installed first, because those
# jobs check the lock in before_script; "post" runs after before_script, which
# is what installs the tool.
lock_cmd() {
  case "$1" in
    node|nestjs|nextjs) echo "pre COREPACK_ENABLE_DOWNLOAD_PROMPT=0 corepack enable && COREPACK_ENABLE_DOWNLOAD_PROMPT=0 pnpm install --lockfile-only" ;;
    fastapi|django) echo "pre \$(grep -o 'pip install.*uv==[0-9.]*' .gitlab-ci.yml | head -1) && uv lock" ;;
    laravel) echo "post composer update --no-install --no-interaction --no-progress" ;;
    *) echo "pre :" ;;
  esac
}

for t in $TEMPLATES; do
  p="$W/ci-$t"
  if [ -e "$p" ]; then echo "FAIL  $t: $p already exists (use a fresh work dir)"; fail=$((fail+1)); continue; fi
  if ! "$B" init -t "$t" --name "ci-$t" "$p" </dev/null >"$W/ci-$t.gen.log" 2>&1; then
    echo "FAIL  $t: generation ($(tail -1 "$W/ci-$t.gen.log"))"; fail=$((fail+1)); continue
  fi
  # One shell script from the two workflow files; the image on the first line.
  if ! python3 - "$p" "$(lock_cmd "$t")" >"$W/ci-$t.job.sh" <<'EOF'
import sys, yaml
proj = sys.argv[1]
lock_when, lock = sys.argv[2].split(" ", 1)
gl = yaml.safe_load(open(f"{proj}/.gitlab-ci.yml"))
reserved = {"image", "stages", "variables", "default", "include", "workflow"}
jobs = {k: v for k, v in gl.items() if k not in reserved and isinstance(v, dict)}
if len(jobs) != 1:
    sys.exit(f"expected one GitLab job, found {sorted(jobs)}")
(name, job), = jobs.items()
image = job.get("image") or gl.get("image")
if not image:
    sys.exit("the GitLab job names no image")
env = {**(gl.get("variables") or {}), **(job.get("variables") or {})}
before = (gl.get("default") or {}).get("before_script", []) + job.get("before_script", [])
gh = yaml.safe_load(open(f"{proj}/.github/workflows/ci.yml"))
runs = []
for jname, j in gh["jobs"].items():
    for k, v in (gh.get("env") or {}).items():
        env.setdefault(k, v)
    for k, v in (j.get("env") or {}).items():
        env.setdefault(k, v)
    for s in j["steps"]:
        if "run" in s:
            runs.append((s.get("name") or s["run"].strip().splitlines()[0], s["run"], s.get("env") or {}))
print(f"# image: {image}")
print("set -e")
def phase(title):
    print(f"echo; echo '##[{title}]'")
# Before the job's variables exist: the lock is what a developer creates on
# their machine, and a job variable such as UV_LOCKED=1 forbids creating it.
if lock_when == "pre":
    phase("create the lock")
    print(lock)
print("export CI=true")
for k, v in env.items():
    print(f"export {k}={str(v)!r}")
phase("before_script")
for c in before:
    print(c)
if lock_when == "post":
    phase("create the lock")
    print(lock)
phase(f"gitlab job '{name}': script")
for c in job["script"]:
    print(c)
for title, body, senv in runs:
    phase("github step: " + title.replace("'", ""))
    print("(")
    for k, v in senv.items():
        print(f"export {k}={str(v)!r}")
    print(body.rstrip())
    print(")")
print("echo; echo '##[done]'")
EOF
  then echo "FAIL  $t: could not read the generated workflows"; fail=$((fail+1)); continue; fi
  image=$(sed -n '1s/^# image: //p' "$W/ci-$t.job.sh")
  cp "$W/ci-$t.job.sh" "$p/.ci-job.sh"
  t0=$(date +%s)
  "$B" container run --rm --net host -v "$p:/builds/project" -w /builds/project "$image" \
    bash /builds/project/.ci-job.sh >"$W/ci-$t.run.log" 2>&1
  rc=$?
  rm -f "$p/.ci-job.sh"
  secs=$(( $(date +%s) - t0 ))
  last=$(grep '^##\[' "$W/ci-$t.run.log" | tail -1)
  steps=$(grep -c '^##\[github step' "$W/ci-$t.run.log")
  if [ $rc = 0 ] && [ "$last" = "##[done]" ]; then
    pass=$((pass+1)); echo "PASS  $t: GitLab job and $steps GitHub run steps in $image (${secs}s)"
  else
    fail=$((fail+1)); echo "FAIL  $t: rc=$rc at $last (${secs}s) — see $W/ci-$t.run.log"
    tail -5 "$W/ci-$t.run.log" | sed 's/^/        /'
  fi
done
echo "== $pass passed, $fail failed"
[ $fail = 0 ]
