<!--
Template for a single finding record (SKILL.md §24–25).
Copy this block once per finding into the relevant report/backlog file.
Every field is required unless marked optional; leave a field explicitly
as "N/A" rather than deleting it, so a reader can tell "not applicable"
from "forgot to fill in."
-->

## <ID> — <Title>

- **ID:** <BUG|GAP|SEC|PERF>-<0000>
- **Group:** <Core Runtime | Containers | Images | VM | Network | Storage | IaC | GitOps | Kubernetes | PaaS | Security | MCP | Provider: <name>>
- **Command / Kind:** <exact command invoked, or Kind under test>
- **Severity:** P0 | P1 | P2 | P3 | P4
- **Status:** OPEN | CONFIRMED | IN_PROGRESS | FIXED | RETEST | CLOSED | WONT_FIX | DUPLICATE | BLOCKED
- **Test ID:** <PREFIX-###, from the taxonomy in references/test-taxonomy.md>
- **Regression?:** yes (introduced in <version>) | no | unknown
- **Environment:** <host, OS, kernel, rootless/root>
- **Version:** <delonix --version output>
- **Commit SHA:** <full or short SHA under test>
- **Provider:** <local | libvirt | Proxmox | OpenStack | N/A>

### Preconditions
<what had to be true or already exist before this test ran>

### Steps to reproduce
```
<exact commands, in order, that reproduce this finding>
```

### Expected result
<what SKILL.md's non-negotiable principle (§3) requires here — the full
effect, not just an exit code>

### Actual result
<what actually happened, described factually>

### Evidence
```
$ <command>
<stdout>
```
```
exit code: <n>
```
```
<relevant stderr / log lines>
```
<resource state before/after, manifest used, provider-side state if applicable>

### Latency
<only if applicable — control-plane vs. provider split per references/performance-slo.md>

### Security impact
<only if this is a SEC finding — attack surface, exploitability, blast radius>

### Root-cause hypothesis
<clearly marked as HYPOTHESIS unless independently confirmed by reading the
actual code path and reproducing the mechanism, in which case mark
CONFIRMED ROOT CAUSE and cite the file/function>

### Suggested fix
<concrete, or "needs design discussion" if the fix isn't obvious>

### Affected components
<crates, CLI groups, API routes, MCP tools, GitOps paths touched>

### Lifecycle
- introduced_version: <version, or "unknown — predates this suite">
- detected_version: <version this finding was first recorded in>
- fixed_version: <filled in when Status moves to FIXED>
- verified_version: <filled in when Status moves to CLOSED after RETEST>

### Regression test
<link/path to the permanent regression test added per SKILL.md §36, or
"not yet added" with a reason if one doesn't exist>
