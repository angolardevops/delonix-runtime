# Performance SLOs — Delonix Runtime E2E

Reference companion to `SKILL.md` §16–17. Defines what to measure, how to
separate control-plane time from provider time, and the regression
thresholds that turn a latency number into a `PERFORMANCE` finding.

## What to capture

For every operation named in `SKILL.md` §16 (runtime/CLI startup,
`ps`/list, `inspect`, container create/start/stop, network create, volume
create, VM create request, API request, GitOps reconciliation, MCP tool
invocation), record across a sample of at least 20 runs where practical:

```
min   p50   p95   p99   max   mean   sample_count
```

A single-run timing is a data point, not a latency claim — report it as
such (`n=1, informational only`) rather than presenting it with the full
percentile table.

## Split control-plane from provider time

Never collapse a multi-stage operation into one number. A VM create's total
wall-clock time is not "VM create latency" — it's several latencies that
belong to different owners:

```
VM create:
  CLI parsing:          8 ms      ← Delonix
  Delonix planning:     20 ms     ← Delonix
  provider request:     65 ms     ← Delonix → provider round trip
  VM provisioning:      8.4 s     ← provider (libvirt/Proxmox/OpenStack)
  guest readiness:      13.2 s    ← guest OS boot, outside Delonix's control
```

Report each stage separately in the finding. A regression in "CLI parsing"
and a regression in "guest readiness" have completely different owners and
completely different fixes — merging them into one number hides which one
actually happened.

## Regression thresholds

Compare the current run's percentile against the stored baseline for the
same operation and the same stage (control-plane vs. provider — never
compare a control-plane number against a provider-inclusive baseline).

| Delta | Classification |
|---|---|
| < 10% | Informational — note it, no finding |
| 10–25% | Warning — record as `PERF-###`, severity P3/P4 |
| 25–50% | Performance gap — `PERF-###`, severity P2 |
| > 50% | Performance bug candidate — `PERF-###`, severity P1 (P0 if it breaks a documented SLA/timeout) |

Context can move a finding up or down a severity from this table — a 60%
regression on an operation nobody times in production (e.g. a rarely-used
`ls-remote`) is not automatically P1, and a 15% regression on `container
run`'s hot path (called dozens of times a day per `main.rs`'s own framing
of the removed root shortcuts) may deserve escalation past what the raw
percentage suggests.

## Baseline management

Store baselines per operation, per stage, per Delonix version, alongside
the environment they were measured on (host, provider, whether the host was
under contention from other work). A baseline captured on a shared,
memory-constrained host is not a fair comparison point for a clean CI
runner — record the environment with the number, and don't compare across
mismatched environments without saying so in the finding.

When a genuine, intentional behavior change explains a regression (a new
default that trades latency for safety, for instance), record it as
`WONT_FIX` with the rationale rather than leaving it open indefinitely —
the backlog should never accumulate permanently-accepted regressions,
whether framed as bugs to fix.

## PERF finding fields (§33)

```
PERF ID:                    PERF-####
Operation:                  <exact command/API path + stage>
Baseline:                   <value, version, environment it was measured on>
Current:                    <value, version, environment>
Delta:                      <percentage, direction>
p50 / p95 / p99:            <full distribution, not just the delta figure>
Suspected bottleneck:       <hypothesis — never presented as confirmed>
Profiling recommendation:   <what to run next to confirm the hypothesis>
Acceptance target:          <the value that would close this finding>
```
