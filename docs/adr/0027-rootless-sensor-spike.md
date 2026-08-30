# ADR-0027: Rootless changes the answer — seccomp user-notification, not eBPF, is this engine's sensor

- **Status:** Proposed (GO/NO-GO spike closed; the decision to BUILD is the owner's)
- **Date:** 2026-08-29
- **Deciders:** Walter (owner)
- **Related:** ADR-0026 (the security runtime is a decision crate), `crates/delonix-net/src/bpf.rs`,
  `crates/delonix-runtime/src/seccomp_profile.rs`, `crates/delonix-runtime/src/lib.rs::log_shim`,
  `docs/adr/README.md` guardrails #1 (daemonless) and #5 (spike before a new privilege boundary).

## Context

ADR-0026 deferred every continuous-detection capability — eBPF sensors, file-integrity
monitoring, malware and behavioural ransomware detection, network anomaly detection — on two
grounds: they need a resident process (guardrail #1), and eBPF was **implied** to be useless
rootless because `delonix-net/src/bpf.rs` documents that loading a program needs `CAP_BPF` +
`CAP_NET_ADMIN`.

That ADR also said, in its own words, that such a design "should not be built on an implication"
and needed a spike measuring what a sensor can actually observe rootless. This is that spike, run
on the engine's own host as the unprivileged user it normally runs as.

**Baseline of the measuring process** — uid 1000, `CapEff: 0000000000000000` (not one effective
capability), kernel 7.0.0-30-generic:

| sysctl | value | consequence |
|---|---|---|
| `kernel.unprivileged_bpf_disabled` | **2** | `bpf()` refused to unprivileged callers, by policy |
| `kernel.perf_event_paranoid` | **4** | perf-based tracing also closed |
| `kernel.yama.ptrace_scope` | 1 | ptrace limited to descendants |

**What an unprivileged sensor can actually open** (probes compiled and run; `EPERM` is the
kernel's answer, not a guess):

| Mechanism | Result | What it means for a sensor |
|---|---|---|
| `bpf(BPF_PROG_LOAD)` | **DENIED** `EPERM` | no eBPF, and not merely for want of `CAP_BPF` — the sysctl bars it |
| `fanotify_init(FAN_CLASS_NOTIF)` | **DENIED** `EPERM` | the classic FIM handle is closed |
| `fanotify_init(FAN_CLASS_CONTENT)` | **DENIED** `EPERM` | no blocking class — cannot refuse a write |
| `fanotify_init(FAN_REPORT_FID)` | **OK** | the unprivileged mode (≥5.13) DOES open |
| ↳ `FAN_MARK_MOUNT` / `FAN_MARK_FILESYSTEM` | **DENIED** `EPERM` | inode marks only — no filesystem-wide watch |
| ↳ `FAN_REPORT_PIDFD` | **DENIED** `EPERM` | **you learn a file changed, never which process changed it** |
| ↳ `FAN_OPEN_PERM` | **DENIED** `EINVAL` | observation only |
| `inotify_init1` | **OK** | 1 048 576 watches, 8 192 instances available |
| `seccomp(SET_MODE_FILTER, NEW_LISTENER)` | **OK** | see below |

**The finding that changes the answer.** A supervisor holding no capability at all was driven
end to end against a child process:

```
supervisor: received the notification fd
supervisor: syscall nr=83 from pid=1114813      <- attributed to a process
supervisor: read the target's argument: "/tmp/spike-should-not-exist"
supervisor: NOTIF_ID_VALID -> OK
supervisor: answered EPERM                     <- REFUSED the operation
result: the workload exited with 1
```

So rootless, with zero capabilities, the engine can **observe a syscall, attribute it to a PID,
read its arguments out of the target's memory, and refuse it**. That is more than eBPF would have
given (which observes but does not enforce), and it is available where eBPF is not.

## Decision

**Close the GO/NO-GO as: eBPF is NO-GO on this engine; seccomp user-notification is the GO
candidate, and file integrity is a partial GO with a stated blind spot.**

1. **eBPF: rejected, and now with a number instead of an implication.** `bpf()` returns `EPERM`
   and `unprivileged_bpf_disabled=2` means no capability grant short of running the engine
   privileged would change it. Any future proposal must first say which of rootless or eBPF it is
   giving up. `delonix-net/src/bpf.rs` stays what it is — optional telemetry for privileged
   installs.
2. **seccomp user-notification is the sensor this engine can actually have**, and it is a natural
   extension rather than a new subsystem: `seccomp_profile.rs` already builds and installs filters
   for every container, so the workload side is one flag (`SECCOMP_FILTER_FLAG_NEW_LISTENER`).
3. **It does NOT need a daemon, which is what makes it admissible under guardrail #1.** The
   listener must be held by a process that outlives the command — and one already exists per
   container: `log_shim` (`crates/delonix-runtime/src/lib.rs`), which already survives `run -d` to
   read the container's stdout/stderr. This is one more responsibility for a process the engine
   already spawns, not a new resident service. A design that instead proposes a host-wide daemon
   is a different decision and needs its own ADR.
4. **File integrity ships, if at all, with its blind spot written on the label.** `FAN_REPORT_FID`
   and `inotify` both work and the watch budget is ample (`/etc` is 437 directories against
   1 048 576 available watches). But `FAN_REPORT_PIDFD` is refused, so **a rootless FIM can say
   that a file changed and cannot say who changed it**, and `FAN_OPEN_PERM` is refused, so it
   cannot prevent the change. An integrity feature that does not name its actor must say so in its
   output; presenting it as attribution would be the overclaim this repo refuses everywhere.

## Alternatives considered

- **Run the engine privileged so eBPF works.** Rejected: rootless is the product's design
  (`AGENTS.md` §2), and trading it for telemetry inverts the security argument — the sensor would
  cost more isolation than it observes.
- **Ship nothing, as ADR-0026 left it.** Rejected now that the measurement exists: ADR-0026
  deferred on the belief that rootless observation was impossible, and that belief is false for
  syscalls. Leaving it deferred would keep a decision resting on an implication the spike has
  disproved.
- **A host-wide sensor daemon.** Rejected for this ADR: guardrail #1, and unnecessary — the
  per-container listener needs no host-wide process.
- **ptrace-based supervision.** Rejected: `yama.ptrace_scope=1` limits it to descendants (which
  would work), but ptrace stops the target on every event and its overhead is the reason the
  kernel grew `seccomp_unotify` in the first place.

## Consequences

**Easier.** A future runtime-detection feature has a mechanism that is measured to work where the
engine actually runs, that already has a home (`log_shim`) and a neighbour (`seccomp_profile.rs`),
and that enforces rather than merely observing.

**Harder, and this is the honest half.**

- **TOCTOU on pointer arguments is real and not fully solvable.** The supervisor reads the path
  out of `/proc/<pid>/mem`; a hostile workload can rewrite that memory after the read and before
  the syscall proceeds. `SECCOMP_IOCTL_NOTIF_ID_VALID` (measured OK above) narrows the window but
  does not close it. **A rule keyed on a pointer argument is therefore an advisory control, not a
  boundary**, and must be written down as such — `seccomp_unotify(2)` says the same. Rules keyed on
  scalar arguments (a syscall number, a flag, a mode) do not have this problem.
- **Every intercepted syscall costs a round trip** to the supervisor. Only syscalls a policy
  actually names should be trapped; trapping broadly would put a userspace hop in a container's hot
  path. No number is offered here because none was measured — that is the next spike, not a claim.
- **The blind spot in FIM is structural**, not a gap to be closed later: `FAN_REPORT_PIDFD` is
  refused by the kernel to an unprivileged process.

**What this ADR does NOT decide.** Whether to build any of it, which syscalls a policy could name,
what the manifest surface would look like, and how a refusal reaches the operator. This closes the
feasibility question that ADR-0026 left open with an implication; the product decision is separate
and belongs to whoever schedules it.

## Reproducing

The three probes ship with this ADR, in
[`scripts/spikes/0027-rootless-sensor/`](../../scripts/spikes/0027-rootless-sensor/) — a claim of
reproducibility with nothing to run would be the same overclaim as an untested control. They need
no privilege and no fixture beyond `/tmp`:

```sh
cd scripts/spikes/0027-rootless-sensor
for p in availability fanotify_scope unotify_end_to_end; do
    gcc -O0 -o /tmp/$p $p.c && /tmp/$p
done
```

Re-run them before trusting the tables above on a different kernel or a host with different
sysctls — every row is a property of THIS host's policy, not of Linux in general.
