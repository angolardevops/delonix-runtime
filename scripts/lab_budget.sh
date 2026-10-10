#!/usr/bin/env bash
# Lab budget preflight: may a live validation boot a guest on this host now?
#
# The completion plan (docs/discovery/68_ENGINE_COMPLETION_PLAN.md, section 4)
# closes every sprint with a live validation on a golden VM or an appliance,
# on a development workstation that other work shares. A validation that
# starves the machine is one nobody runs twice, and a result measured on a
# starved host gets blamed on the engine. So the check refuses instead of
# squeezing, and says the three numbers it decided on.
#
# Usage: scripts/lab_budget.sh [--ram GiB] [--vcpus N]
#   --ram    RAM the guest about to boot asks for (default 4)
#   --vcpus  vCPUs the guest about to boot asks for (default 2)
#
# Exit: 0 go; 3 refused (the host is short of something — wait, do not
# retry in a loop); 2 usage error. The thresholds are environment variables so
# a lab host with more room can say so:
#   LAB_MIN_FREE_RAM_GIB  RAM that must stay free AFTER the guest (default 4)
#   LAB_MIN_FREE_DISK_GIB free disk on the state root's filesystem (default 30)
#   LAB_MAX_LOAD          load(1m) ceiling (default: 3/4 of the threads)
#   LAB_MAX_VCPUS         vCPUs one guest may take (default 2)
#   LAB_MAX_GUEST_RAM_GIB RAM one guest may take (default 4)
# The inputs can be redirected for the tests: LAB_MEMINFO, LAB_LOADAVG,
# LAB_DISK_PATH, LAB_NPROC.
set -euo pipefail

want_ram=4
want_vcpus=2
while [[ $# -gt 0 ]]; do
  case "$1" in
    --ram) want_ram="${2:?--ram needs a value}"; shift 2 ;;
    --vcpus) want_vcpus="${2:?--vcpus needs a value}"; shift 2 ;;
    -h|--help) sed -n '2,25p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "lab_budget: unknown argument: $1" >&2; exit 2 ;;
  esac
done
for v in "$want_ram" "$want_vcpus"; do
  [[ "$v" =~ ^[0-9]+$ ]] || { echo "lab_budget: not a whole number: $v" >&2; exit 2; }
done

meminfo="${LAB_MEMINFO:-/proc/meminfo}"
loadavg="${LAB_LOADAVG:-/proc/loadavg}"
disk_path="${LAB_DISK_PATH:-${DELONIX_ROOT:-${XDG_DATA_HOME:-$HOME/.local/share}/delonix}}"
threads="${LAB_NPROC:-$(nproc)}"

# The state root may not exist yet on a fresh lab; measure the nearest parent.
while [[ ! -e "$disk_path" && "$disk_path" != / ]]; do disk_path="$(dirname "$disk_path")"; done

avail_kib=$(awk '/^MemAvailable:/ {print $2}' "$meminfo")
[[ -n "$avail_kib" ]] || { echo "lab_budget: no MemAvailable in $meminfo" >&2; exit 3; }
avail_gib=$(( avail_kib / 1024 / 1024 ))
disk_gib=$(df -P -k "$disk_path" | awk 'NR==2 {print int($4 / 1024 / 1024)}')
load1=$(awk '{print $1}' "$loadavg")

min_free_ram="${LAB_MIN_FREE_RAM_GIB:-4}"
min_disk="${LAB_MIN_FREE_DISK_GIB:-30}"
max_load="${LAB_MAX_LOAD:-$(( threads * 3 / 4 ))}"
max_vcpus="${LAB_MAX_VCPUS:-2}"
max_guest_ram="${LAB_MAX_GUEST_RAM_GIB:-4}"

echo "lab_budget: ram ${avail_gib} GiB available, disk ${disk_gib} GiB free (${disk_path}), load ${load1} on ${threads} threads; guest asks ${want_vcpus} vCPU / ${want_ram} GiB"

refused=()
(( want_vcpus <= max_vcpus )) || refused+=("the guest asks ${want_vcpus} vCPUs, the budget is ${max_vcpus}")
(( want_ram <= max_guest_ram )) || refused+=("the guest asks ${want_ram} GiB, the budget is ${max_guest_ram}")
(( avail_gib - want_ram >= min_free_ram )) || refused+=("${avail_gib} GiB available leaves less than ${min_free_ram} GiB after a ${want_ram} GiB guest")
(( disk_gib >= min_disk )) || refused+=("${disk_gib} GiB free on disk, the budget needs ${min_disk}")
awk -v l="$load1" -v m="$max_load" 'BEGIN { exit !(l < m) }' || refused+=("load ${load1} is not below ${max_load}")

if (( ${#refused[@]} )); then
  for r in "${refused[@]}"; do echo "lab_budget: REFUSED — $r" >&2; done
  exit 3
fi
echo "lab_budget: go"
