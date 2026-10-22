# shellcheck shell=bash
# Host-port picker for the e2e battery. Sourced by `scripts/e2e.sh`, which also
# proves it in its first section; the caller defines `log`.

# e2e_port <var> — set <var> to a port that is free on this host right now and
# lies outside the kernel's ephemeral range.
#
# The battery used to draw its published ports with `BASE + RANDOM % N`, and one
# base (51800) sat inside `ip_local_port_range` (32768–60999 by default). There
# the port can be the local end of an outgoing connection the host already has:
# on 2026-10-21 it drew 51814 while 127.0.0.1:51814↔:5433 was open, the engine
# rightly refused the publish with EADDRINUSE, and that cascaded into 7 FAILs
# that had nothing to do with the diff. Outside the ephemeral range the kernel
# never picks a port for a `connect()`, so only a socket someone bound on
# purpose can hold it — and the probe sees that one.
#
# The window is read from the host, not assumed: [20000, lo) when the range
# starts high enough, else (hi, 65535]. Each candidate is probed with a real
# bind — TCP and UDP, on 0.0.0.0 and on :: — without SO_REUSEADDR, so a port any
# socket holds (listening or connected) is refused, and a port already handed
# out in this run is never handed out twice. After `E2E_PORT_TRIES` misses it
# asks the kernel (bind to port 0) and probes that one: it is inside the
# ephemeral range, but free when measured, which beats failing the run.
# `E2E_PORT_MIN`/`E2E_PORT_MAX` override the window, for the scenario that
# proves the fallback.
#
# The ports some sections hard-code are seeded as taken so a drawn port never
# lands on them.
E2E_PORTS_GIVEN=" 19080 19099 19443 31080 "

_e2e_port_free() {
  python3 -I - "$1" <<'PY'
import errno, socket, sys
port = int(sys.argv[1])
for fam, addr in ((socket.AF_INET, "0.0.0.0"), (socket.AF_INET6, "::")):
    for kind in (socket.SOCK_STREAM, socket.SOCK_DGRAM):
        try:
            s = socket.socket(fam, kind)
        except OSError:
            continue  # no IPv6 on this host
        try:
            if fam == socket.AF_INET6:
                s.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_V6ONLY, 1)
            s.bind((addr, port))
        except OSError as e:
            # IPv6 disabled by sysctl binds with EADDRNOTAVAIL: not a holder.
            if fam == socket.AF_INET or e.errno == errno.EADDRINUSE:
                sys.exit(1)
        finally:
            s.close()
PY
}

_e2e_port_kernel() {
  python3 -I -c 'import socket; s=socket.socket(); s.bind(("0.0.0.0", 0)); print(s.getsockname()[1]); s.close()'
}

e2e_port() {
  local _ep_var="$1" _ep_lo _ep_hi _ep_min _ep_max _ep_p _ep_try
  if ! read -r _ep_lo _ep_hi </proc/sys/net/ipv4/ip_local_port_range 2>/dev/null; then _ep_lo=32768; _ep_hi=60999; fi
  if (( _ep_lo - 20000 >= 1000 )); then _ep_min=20000; _ep_max=$((_ep_lo - 1))
  elif (( 65535 - _ep_hi >= 1000 )); then _ep_min=$((_ep_hi + 1)); _ep_max=65535
  else _ep_min=; _ep_max=
  fi
  _ep_min=${E2E_PORT_MIN:-$_ep_min}; _ep_max=${E2E_PORT_MAX:-$_ep_max}
  if [[ -n "$_ep_min" && -n "$_ep_max" ]]; then
    for (( _ep_try = 0; _ep_try < ${E2E_PORT_TRIES:-40}; _ep_try++ )); do
      _ep_p=$(( _ep_min + ((RANDOM << 15) | RANDOM) % (_ep_max - _ep_min + 1) ))
      [[ "$E2E_PORTS_GIVEN" == *" $_ep_p "* ]] && continue
      _e2e_port_free "$_ep_p" || continue
      E2E_PORTS_GIVEN+="$_ep_p "; printf -v "$_ep_var" '%s' "$_ep_p"; return 0
    done
  fi
  for (( _ep_try = 0; _ep_try < 10; _ep_try++ )); do
    _ep_p=$(_e2e_port_kernel) || continue
    [[ "$E2E_PORTS_GIVEN" == *" $_ep_p "* ]] && continue
    _e2e_port_free "$_ep_p" || continue
    log "  (portas: nenhuma livre em ${_ep_min:-?}–${_ep_max:-?} fora do intervalo efémero $_ep_lo–$_ep_hi; $_ep_var=$_ep_p veio do kernel)"
    E2E_PORTS_GIVEN+="$_ep_p "; printf -v "$_ep_var" '%s' "$_ep_p"; return 0
  done
  log "FATAL: e2e_port: nenhuma porta livre para $_ep_var (janela ${_ep_min:-?}–${_ep_max:-?}, efémero $_ep_lo–$_ep_hi)"
  exit 2
}
