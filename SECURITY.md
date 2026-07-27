# Security Policy

Delonix Runtime is a rootless-first container and microVM engine that runs on production hosts
with real namespace/cgroup/nftables privileges. A vulnerability here isn't "a bug" — it can mean a
namespace escape, a rootless→root privilege escalation, or arbitrary code execution on a host that
never gave you interactive access. We take reports in this category seriously and ask that you do
too, by not disclosing them publicly before a fix is available.

## Reporting a vulnerability

**Please do not open a public GitHub issue for security vulnerabilities.**

Use **[GitHub Private Vulnerability Reporting](https://github.com/angolardevops/delonix-runtime/security/advisories/new)**
(enabled on this repository) — go to the **Security** tab → **Report a vulnerability**. This opens
a private draft advisory visible only to you and the maintainers, with no public trace until it's
resolved and you both agree to disclose.

Include, if you can:
- The affected version (`delonix --version`) and whether the issue requires rootless or root mode
- A minimal reproduction (command, manifest, or config that triggers it)
- The concrete impact — what an attacker gains, not just "this looks unsafe"
- Whether it's remotely triggerable (e.g. via a manifest from an untrusted source, a malicious
  container image, or network input) vs. requiring local access

## What's in scope

- Privilege escalation (rootless → root, container → host)
- Namespace/cgroup escapes
- Command injection (especially anything reaching a `sudo`/SSH-driven remote command, e.g. in
  `cluster apply`/`cluster kubeadm`)
- Path traversal in image extraction, `COPY`/build contexts, volume/snapshot names, or manifest-
  driven file operations
- Authentication/authorization bypass on the control socket, the management API, or the CRI socket
- Supply-chain issues (unverified downloads, missing digest/checksum verification)
- Memory-safety issues in any `unsafe` block that aren't already covered by Rust's guarantees

## What's out of scope

- Denial of service from a container you already control (resource exhaustion inside your own
  container is expected — cgroup limits are opt-in via `--memory`/`--cpus`, not a security
  boundary by default)
- Issues that require the attacker to already have root on the host
- Vulnerabilities in third-party base images you choose to run
- Missing hardening best-practices with no concrete exploit path

If you're not sure whether something qualifies, report it privately anyway — worst case we
downgrade it to a normal issue together.

## Our process

1. We acknowledge new reports as promptly as we can.
2. We confirm the issue, assess severity, and work on a fix in the private advisory.
3. We coordinate a disclosure timeline with you — we default to fixing and releasing before public
   disclosure, and crediting you in the advisory and release notes unless you'd rather stay
   anonymous.
4. Once a fix ships, the advisory (and a CVE, where warranted) is published.

## Past security work

This project has had multiple rounds of dedicated offensive security review (command injection,
namespace/privilege-escalation boundaries, memory safety, supply-chain verification, path
traversal) — see the "Auditoria de segurança" sections in
[CLAUDE.md](CLAUDE.md) for what's already been reviewed and fixed. That history doesn't mean
new code is exempt — it means we take this seriously and expect the same from reports.
