<!--
Templates for the backlog family (SKILL.md §30-34):
  reports/e2e/<version>/BUG_BACKLOG.md
  reports/e2e/<version>/GAP_BACKLOG.md
  reports/e2e/<version>/SECURITY_BACKLOG.md
  reports/e2e/<version>/PERFORMANCE_BACKLOG.md
  reports/e2e/<version>/BACKLOG.md   (master, merges all four)
Each section below is one of those files' body. The master backlog
interleaves all four by severity (P0 → P4) rather than keeping them in
separate severity-sorted lists — a P0 SEC item outranks a P1 BUG.
-->

# BUG_BACKLOG.md

Grouped by severity. Within a severity, order by group (SKILL.md §30 order).

## P0

### <BUG-####> — <title>
- **Component:** <crate/CLI group/API route>
- **Owner:** <unassigned>
- **Impact:** <what breaks for a real user/operator>
- **Reproduction:** <link to the finding, templates/finding.md>
- **Acceptance criteria:** <what "fixed" means, testable>
- **Related tests:** <test IDs>
- **Dependencies:** <blocked by / blocks>
- **Estimated complexity:** XS | S | M | L | XL

<repeat per P0/P1/P2/P3/P4 item>

---

# GAP_BACKLOG.md

## P0
### <GAP-####> — <title>
- **Current behaviour:** <what Delonix does today>
- **Expected platform behaviour:** <what a mature platform in the same
  space does, per the comparison lenses in SKILL.md §1/§39 — cite which>
- **Why it matters:** <concrete user/operator scenario this blocks>
- **Suggested design:** <sketch, or "needs design discussion">
- **Affected interfaces:** CLI | API | MCP | GitOps <mark all that apply>
- **Possible breaking changes:** <yes/no, and what breaks>
- **Acceptance criteria:** <testable definition of done>

<repeat per severity>

---

# SECURITY_BACKLOG.md

## P0
### <SEC-####> — <title>
- **Risk:** <one line>
- **Attack surface:** <where this is reachable from>
- **Affected component:** <crate/route/CLI path>
- **Exploitability:** <what a caller needs to trigger this>
- **Impact:** <blast radius if exploited>
- **Recommended mitigation:** <concrete fix, not a restated problem>
- **Validation test:** <how to confirm the mitigation closes it>
- **Security regression test:** <path once added, per SKILL.md §36>

<repeat per severity>

---

# PERFORMANCE_BACKLOG.md

## P0
### <PERF-####> — <operation>
- **Baseline:** <value, version, environment>
- **Current:** <value, version, environment>
- **Delta:** <percentage>
- **p50 / p95 / p99:** <full distribution>
- **Suspected bottleneck:** <hypothesis, marked as such>
- **Profiling recommendation:** <what to run to confirm it>
- **Acceptance target:** <the number that closes this>

<repeat per severity>

---

# BACKLOG.md (master)

All findings from the four backlogs above, interleaved by severity only —
a P0 SEC item is listed before a P1 BUG, regardless of category.

## P0
- [ ] <SEC-####> — <title> — *(security)*
- [ ] <BUG-####> — <title> — *(bug)*
- [ ] <GAP-####> — <title> — *(gap)*
- [ ] <PERF-####> — <title> — *(performance)*

## P1
<same interleaving>

## P2
<same interleaving>

## P3
<same interleaving>

## P4
<same interleaving>

## Closed since last run
<items moved to CLOSED this run, with fixed_version/verified_version, so a
reader can see what improved without re-reading every finding>
