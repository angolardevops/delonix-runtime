# Spike ADR-0027 — what a security sensor can observe, rootless

Three probes behind [ADR-0027](../../../docs/adr/0027-rootless-sensor-spike.md).
They need **no privilege** and no fixture beyond `/tmp`.

```sh
for p in availability fanotify_scope unotify_end_to_end; do
    gcc -O0 -o /tmp/$p $p.c && /tmp/$p
done
```

- `availability.c` — which sensor handles a capability-less process can open at all
  (`bpf`, `fanotify` in each class, `inotify`, `seccomp` with a listener).
- `fanotify_scope.c` — how far the *unprivileged* fanotify handle reaches: inode
  marks only, or mount/filesystem too, and whether it can refuse an open.
- `unotify_end_to_end.c` — a supervisor with zero capabilities observing a child's
  syscall, attributing it to a PID, reading its argument, and refusing it.

Every row in the ADR's tables is a property of **the host it was run on** — its
`kernel.unprivileged_bpf_disabled`, `perf_event_paranoid` and kernel version — not
of Linux in general. Re-run before trusting the table elsewhere.
