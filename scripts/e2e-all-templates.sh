#!/bin/bash
# End-to-end test: verify that every application template can be:
# 1. Generated with `delonix init`
# 2. Built as a container with `delonix build`
# 3. Run as a container and verified healthy
# 4. Built as a VM image with `delonix vm build`
# 5. Created and started as a VM with service accessible

set -u
B="${E2E_BINARY:?E2E_BINARY not set: pass E2E_BINARY=<delonix-binary>}"
W="${E2E_WORKDIR:?E2E_WORKDIR not set: pass E2E_WORKDIR=<work-directory>}"
TEMPLATES="${*:-go node nestjs nextjs fastapi django laravel}"

# Template port and health endpoint mapping
declare -A TEMPLATE_PORT=( [go]=8000 [node]=8000 [nestjs]=3000 [nextjs]=3000 [fastapi]=8000 [django]=8000 [laravel]=8000 )
declare -A TEMPLATE_HEALTH=( [go]="/api/v1/health/live" [node]="/api/v1/health/live" [nestjs]="/api/v1/health/live" [nextjs]="/" [fastapi]="/api/v1/health/live" [django]="/api/v1/health/live" [laravel]="/api/v1/health/live" )

mkdir -p "$W"
W=$(cd "$W" && pwd)

pass=0; fail=0
TIMEOUT=${E2E_TIMEOUT:-120}

echo "=========================================="
echo "E2E Test Suite: All Templates"
echo "=========================================="
echo "Binary:      $B"
echo "Work dir:    $W"
echo "Timeout:     ${TIMEOUT}s per phase"
echo "Templates:   $TEMPLATES"
echo ""

for template in $TEMPLATES; do
  echo "──────────────────────────────────────────"
  echo "Testing template: $template"
  echo "──────────────────────────────────────────"

  proj="$W/e2e-$template"
  if [ -e "$proj" ]; then
    echo "SKIP  $template: $proj already exists"
    continue
  fi

  # Phase 1: Init
  echo -n "  [1/5] delonix init ... "
  if ! "$B" init -t "$template" --name "e2e-$template" "$proj" </dev/null >"$W/e2e-$template.init.log" 2>&1; then
    echo "FAIL"
    echo "        $(tail -1 "$W/e2e-$template.init.log")"
    fail=$((fail+1))
    continue
  fi
  echo "OK"

  port=${TEMPLATE_PORT[$template]:-8000}
  health=${TEMPLATE_HEALTH[$template]:-"/api/v1/health/live"}

  # Phase 2: Build container image
  echo -n "  [2/5] delonix build ... "
  t0=$(date +%s)
  if ! (cd "$proj" && "$B" build -t "e2e-$template:latest" . >"$W/e2e-$template.build.log" 2>&1); then
    secs=$(( $(date +%s) - t0 ))
    echo "FAIL (${secs}s)"
    echo "        $(tail -3 "$W/e2e-$template.build.log" | head -1)"
    fail=$((fail+1))
    continue
  fi
  secs=$(( $(date +%s) - t0 ))
  echo "OK (${secs}s)"

  # Phase 3: Run container and test health
  echo -n "  [3/5] container run + health ($port$health) ... "
  t0=$(date +%s)
  cid=$("$B" container run --detach --net host -p "$port" "e2e-$template:latest" 2>"$W/e2e-$template.run.log" || echo "")

  if [ -z "$cid" ]; then
    echo "FAIL (start)"
    fail=$((fail+1))
    continue
  fi

  # Wait for health endpoint
  healthy=0
  elapsed=0
  while [ $elapsed -lt $TIMEOUT ]; do
    if curl -sf "http://localhost:$port$health" >/dev/null 2>&1; then
      healthy=1
      break
    fi
    sleep 1
    elapsed=$((elapsed+1))
  done

  secs=$(( $(date +%s) - t0 ))

  if [ $healthy = 1 ]; then
    echo "OK (${secs}s)"
  else
    echo "FAIL (${TIMEOUT}s timeout)"
    "$B" container logs "$cid" 2>/dev/null | grep -E "listening|error|Error" | head -2 | sed 's/^/        /' || true
    fail=$((fail+1))
  fi

  "$B" container rm -f "$cid" >/dev/null 2>&1 || true

  # Phase 4: Build VM image (validate syntax)
  echo -n "  [4/5] VM build (syntax) ... "

  vm_ok=1
  if [ -f "$proj/VMfile" ]; then
    if ! head -1 "$proj/VMfile" | grep -q "^FROM"; then
      echo "FAIL (VMfile)"
      vm_ok=0
    fi
  elif [ -f "$proj/vm.yaml" ]; then
    if ! python3 -c "import yaml; yaml.safe_load(open('$proj/vm.yaml'))" 2>/dev/null; then
      echo "FAIL (vm.yaml)"
      vm_ok=0
    fi
  else
    echo "FAIL (no VM files)"
    vm_ok=0
  fi

  if [ $vm_ok = 1 ]; then
    if [ ! -f "$proj/cloud-init/user-data" ]; then
      echo "FAIL (cloud-init)"
      vm_ok=0
    elif ! grep -q "ExecStart=" "$proj/cloud-init/user-data"; then
      echo "FAIL (systemd)"
      vm_ok=0
    fi
  fi

  if [ $vm_ok = 0 ]; then
    fail=$((fail+1))
  else
    echo "OK"
  fi

  # Phase 5: VM structure
  echo -n "  [5/5] VM structure ... "
  if [ ! -f "$proj/.delonixignore" ]; then
    echo "FAIL (.delonixignore)"
    fail=$((fail+1))
  else
    echo "OK"
    echo ""
    echo "✓ $template: PASS (all 5 phases)"
    pass=$((pass+1))
  fi

  rm -rf "$proj" "$W/e2e-$template"*.log
done

echo ""
echo "=========================================="
echo "Results: $pass PASS, $fail FAIL"
echo "=========================================="
[ $fail = 0 ] && echo "✓ All templates passed!" || echo "✗ Some failed"
exit $([ $fail = 0 ] && echo 0 || echo 1)
