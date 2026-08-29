# Test Taxonomy — Delonix Runtime E2E

Reference companion to `SKILL.md`. Defines the test groups, stable ID
prefixes, and the shape every individual test case must take. Load this when
building or extending the test inventory — `SKILL.md` says *what* to cover,
this says *how each test is named and structured*.

## Groups and ID prefixes

Every test belongs to exactly one group and gets a stable, sequential ID
within that group's prefix. IDs are never reused, even for a test later
deleted — a gap in the sequence is cheaper than a collision with history.

| Prefix | Group | SKILL.md section |
|---|---|---|
| `CORE-###` | Core Runtime (version, info, doctor, config, health) | 6.A |
| `CTR-###` | Containers | 6.B |
| `IMG-###` | Images | 6.C |
| `VM-###` | Virtual Machines | 6.D |
| `NET-###` | Networking | 6.E |
| `STO-###` | Storage | 6.F |
| `IAC-###` | IaC (manifests, VMfile, Stack, Terraform/OpenTofu) | 6.G |
| `GITOPS-###` | GitOps | 6.H |
| `K8S-###` | Kubernetes / KaaS | 6.I |
| `PAAS-###` | PaaS | 6.J |
| `SEC-###` | Security (see `security-checks.md`) | 15 |
| `MCP-###` | MCP tool surface | — |
| `PROXMOX-###` | Proxmox provider | 6.K |
| `LIBVIRT-###` | libvirt provider | 6.K |
| `OPENSTACK-###` | OpenStack provider | 6.K |
| `PERF-###` | Performance (see `performance-slo.md`) | 16–17 |

A test that spans two groups (e.g. a VM on a specific provider) gets an ID in
the more specific group (`LIBVIRT-###`), not the generic one.

## Test spec format (per SKILL.md §43)

Every test case, regardless of group, is written in this shape before it is
executed — not filled in retroactively:

```
Test ID:                 <PREFIX-###>
Title:                   <one line, states the expected behaviour>
Domain:                  <group>
Preconditions:           <what must already exist/be true>
Setup:                   <resources created for this test, prefixed dlx-e2e-*>
Command:                 <exact CLI invocation or API call>
Expected State:          <the state that must exist after the command>
Assertions:              <checks against Expected State, one per line>
Security Assertions:     <if applicable — isolation, permission, no leak>
Performance Assertions:  <if applicable — latency budget, see performance-slo.md>
Cleanup:                 <how the setup resources are torn down>
Result:                  OK | BUG | GAP | SEC | PERFORMANCE | BLOCKED | SKIPPED
Evidence:                <command, exit code, stdout/stderr, logs, timing>
```

## The five test shapes every command needs (§9–14)

A command is not considered validated until all five shapes below have been
attempted for it. "Not applicable" is a valid outcome for some shapes on some
commands, but it must be stated, not silently skipped.

1. **Positive** (§9) — the documented, expected-to-succeed scenarios:
   minimal valid input, full valid input, every documented flag combination
   that makes sense together.
2. **Negative** (§10) — every documented or inferable way the command should
   *refuse*: missing required parameter, invalid value, unknown resource,
   duplicate name, conflicting state, missing dependency, unavailable
   provider. The command must fail safely — no partial state, no crash, a
   message that says what to do next.
3. **Edge case** (§11) — the boundary conditions nobody writes a docs example
   for: empty strings, very long names, Unicode, embedded spaces, maximum
   and minimum values, zero, repeated invocation, operating on a resource
   that already exists or was already deleted, a process killed mid-operation.
4. **Idempotency** (§12) — for anything declarative (`apply`, `stack apply`,
   manifest-driven `create`): applying the same input twice produces no
   unintended change on the second application. Then: change one field,
   re-plan, verify the diff names exactly that field, apply, verify the
   resulting state matches.
5. **Concurrency** (§13) — run the same or related operations in parallel
   (multiple creates, parallel pulls, simultaneous network ops) and check
   for deadlock, race, duplicate resource creation, or state corruption
   before trusting a command under real multi-caller load.

## Recovery testing (§14)

For each group that manages long-lived state (containers, VMs, networks,
volumes, GitOps sync), at least one test interrupts the operation
mid-flight — kill the engine process, disconnect the provider, cut network
connectivity — and then validates, after restart:

- state consistency (no half-created resource presented as complete);
- no orphaned resource left behind with nothing pointing at it;
- no incomplete transaction silently treated as done;
- automatic reconciliation where the engine claims it, and a clear error
  where it does not.

## Resource leak checklist (§18)

Run after every group, not just at the end of the whole suite. A group is not
clean merely because its own commands returned success — check independently
for what any command in the group might have left behind:

processes, containers, VMs, network namespaces, bridges, veth pairs, tap
devices, volumes, mounts, temporary files, locks, sockets,
iptables/nftables rules, eBPF programs, provider-side resources (VMs on
Proxmox/OpenStack that Delonix itself no longer tracks).
