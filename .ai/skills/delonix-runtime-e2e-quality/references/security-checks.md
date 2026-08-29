# Security Checks — Delonix Runtime E2E

Reference companion to `SKILL.md` §15. Findings from this checklist are
classified `SEC`, tracked separately from functional `BUG`/`GAP` findings,
and feed `SECURITY_BACKLOG.md` (see `templates/backlog.md`).

## Scope discipline (§15, §40)

Every check here runs against an **isolated test environment**, never a host
or tenant that carries real data. Identify the environment before running
anything destructive. Resources this suite creates are named `dlx-e2e-*`
with a run ID (`dlx-e2e-20260829-network-01`), and cleanup never removes a
resource that suite didn't create, unless it is an explicitly marked fixture.

This checklist documents attack surface and validates defenses that already
exist in Delonix's own threat model — it is not a request for novel exploit
development, and no finding here should include weaponized instructions
beyond what is needed to reproduce and fix the issue.

## Checklist

Group each check under the area it tests, and record a test ID
(`SEC-###`) whether it passes or fails — a passing security check is
evidence the property holds today, not just an absence of a finding.

### Privilege and isolation
- Privilege escalation from a rootless container/VM context.
- Unsafe capabilities granted where a narrower one would do.
- Container breakout protections (namespace, cgroup, seccomp/AppArmor
  where applicable) hold under a deliberately hostile workload.
- Tenant isolation: a resource, network, or volume owned by tenant A is
  unreachable and unlistable by tenant B, at every surface (CLI with a
  tenant-scoped token, API, GitOps).
- Network isolation between tenants holds under direct IP addressing, not
  just the documented service names.
- RBAC bypass: a token scoped to one role cannot reach a route or resource
  reserved for a higher one.
- Unsafe VM access: console, VNC, SSH bootstrap paths don't leak host
  credentials or grant unintended host-side access.

### Input handling
- Arbitrary shell execution via a manifest field, CLI argument, or webhook
  payload that gets interpolated into a command.
- Command injection through resource names, labels, tags, or paths.
- Path traversal in any file-accepting field (`-f`, volume paths, backup
  destinations, VMfile `COPY` sources).
- Symlink attacks — a path that resolves outside the expected root at the
  time of use, not just at validation time (TOCTOU).
- Malformed or malicious manifests: oversized documents, deeply nested
  structures, unknown fields that get silently accepted somewhere they
  shouldn't, a `kind` or `apiVersion` crafted to exploit a lax matcher.
- Malformed API payloads: type confusion, missing required fields treated
  as valid, unbounded array/string fields.

### Secrets and credentials
- Secret leakage in logs, error messages, `stack describe`/`inspect`
  output, or generated files (cloud-init, VMfile scaffolds).
- Credential leakage across process boundaries (env vars visible to
  unrelated children, secrets in `/proc/<pid>/cmdline`).
- Secret masking actually redacts in every output mode (`table`, `-o json`,
  `--l18n=pt`), not only the default.
- Unsafe temporary files: secrets or credentials written to a
  world-readable temp path, even briefly.
- World-writable files created by any provisioning or scaffold step.
- Insecure defaults: does a freshly-created resource start locked down, or
  does it need an explicit flag to not be open?

### Supply chain
- Signed images are verified when verification is requested — an
  unsigned or tampered image is rejected, not silently accepted.
- Untrusted image sources (arbitrary registries, `--url-img`) get the
  documented trust treatment (checksum/signature check, or an explicit
  "trusting TLS only" warning) rather than silent trust.
- SBOM generation, where claimed, reflects the actual dependency graph.

### AI / MCP surface
- MCP authorization: a tool call cannot act outside the scope the calling
  session was granted.
- AI-destructive actions (delete, prune, force-remove) go through the same
  confirmation/dry-run path a human CLI user would hit, not a silent
  bypass because the caller is an agent.

## Severity mapping

Use §22 of `SKILL.md`. As a rule of thumb for security findings
specifically:

- **P0** — host compromise, tenant escape, secret exfiltration reachable
  without prior authentication.
- **P1** — RBAC/tenant-isolation bypass reachable with a valid but
  under-privileged credential; unsigned/untrusted image silently trusted
  where verification was requested.
- **P2** — a defense-in-depth control missing but the primary control
  still holds (e.g. secret redaction misses one output mode, but the
  secret store itself is not exposed).
- **P3** — a hardening gap with no demonstrated path to impact yet.

## Regression rule

Per `SKILL.md` §36, every confirmed `SEC` finding gets a permanent
regression test before it is closed — the same input, re-run automatically,
that first exposed the issue.
