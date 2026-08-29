<!--
Template for reports/e2e/<version>/delonix-e2e-report.md (SKILL.md §26–29).
Fill every section from actually-executed tests. A row with no test run
behind it is BLOCKED or SKIPPED with a reason, never silently omitted.
-->

# Delonix Runtime E2E Validation Report

## Executive Summary

| | |
|---|---|
| Version | <delonix --version> |
| Commit | <SHA> |
| Date | <ISO date> |
| Environment | <host description, isolated/shared, resource limits> |
| Providers | <which providers were configured and tested this run> |
| Duration | <wall clock time of the full suite> |
| Commands discovered | <count, from a fresh `--help` walk, not assumed> |
| Commands tested | <count> |
| Kinds discovered | <count, from the repository, not assumed> |
| Kinds tested | <count> |

| Result | Count |
|---|---|
| Total tests | <n> |
| OK | <n> |
| BUG | <n> |
| GAP | <n> |
| SEC | <n> |
| PERFORMANCE | <n> |
| BLOCKED | <n> |
| SKIPPED | <n> |
| **Pass rate** | <OK / (Total − SKIPPED − BLOCKED)> |

## Grouped Results

One subsection per group from `references/test-taxonomy.md`. Every group
that has any discovered surface appears here, even if every test in it was
`SKIPPED`/`BLOCKED` — say why.

### <Group name>

| | |
|---|---|
| Tests | <n> |
| OK / BUG / GAP / SEC / PERFORMANCE | <n> / <n> / <n> / <n> / <n> |
| Pass % | <percentage> |
| Mean latency | <value, or N/A> |
| p95 latency | <value, or N/A> |

<repeat per group: Core Runtime, Containers, Images, VM, Network, Storage,
IaC, GitOps, Kubernetes, PaaS, Security, MCP, Provider: <name> (one per
configured provider)>

## Command Matrix

Every discovered command appears here — generated dynamically from the
current `--help` tree (SKILL.md §4), never from a remembered list.

| Command | Functional | Error Handling | Sec | Perf | Result |
|---|---|---|---|---|---|
| <delonix group verb> | OK / BUG-#### | OK / GAP-#### | OK / SEC-#### | <latency> | OK / BUG / GAP / SEC |

## Kind Matrix

Every discovered Kind appears here — generated from the repository's own
Kind registry (SKILL.md §5), never assumed from documentation.

| Kind | Schema | Create | Update | Delete | GitOps | Security | Result |
|---|---|---|---|---|---|---|---|
| <Kind> | <stable/unstable/none> | OK/BUG/GAP | OK/BUG/GAP | OK/BUG/GAP | OK/GAP/N-A | OK/SEC | <overall> |

## Cleanup Verification

| | |
|---|---|
| Expected resources remaining | <count — fixtures deliberately kept, if any> |
| Unexpected resources | <count> |
| Leaks detected | <count, with links to the findings that document each> |

## Terminal Summary (SKILL.md §49)

```
DELONIX RUNTIME E2E

Version: <version>
Commit: <sha>

Tests:       <n>
OK:          <n>
BUG:         <n>
GAP:         <n>
SEC:         <n>
PERFORMANCE: <n>
BLOCKED:     <n>
SKIPPED:     <n>

Pass Rate: <pct>%

P0: <n>
P1: <n>
P2: <n>
P3: <n>
P4: <n>

Latency regressions: <n>
Resource leaks: <n>

Release Recommendation:

GO | NO-GO

Blocking:
<list of P0/critical-P1 finding IDs, or "none">

Report:
reports/e2e/<version>/delonix-e2e-report.md

Backlog:
reports/e2e/<version>/BACKLOG.md
```
