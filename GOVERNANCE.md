# Governance

Delonix Runtime is maintained by one person today — see [MAINTAINERS.md](MAINTAINERS.md). This
document describes how decisions get made now, and how that changes if the maintainer list grows.
It says what's actually true of this project, not what a governance template usually says: a
single-maintainer BDFL model, not a foundation or a voting committee, because that would be a
process invented for a size this project isn't yet.

## Two kinds of decision

**Structural decisions** — anything that changes the shape of the engine (a new privilege
boundary, a new Kind in the manifest, dropping/renaming public CLI surface, a dependency the
whole workspace would carry) — go through an **Architecture Decision Record**
(`docs/adr/NNNN-title.md`). The record states the context, the decision, the alternatives
considered and rejected, and the consequences; it is never rewritten after acceptance — a changed
mind gets a new, superseding ADR. See [docs/adr/README.md](docs/adr/README.md) for the index and
the non-negotiable guardrails every ADR has to respect (daemonless by design, no
tenant/licence/billing coupling, no private-repo dependency, a GO/NO-GO spike before any new
privilege boundary, no silent failure).

**Everything else** — a bug fix, a new flag on an existing command, a test, a doc fix — is a
normal pull request, reviewed and merged the way [CONTRIBUTING.md](CONTRIBUTING.md) describes.
No ADR needed.

## Who decides

Today: the maintainer. An ADR is accepted when the maintainer accepts it; a PR is merged when the
maintainer merges it. There is no vote to hold and no quorum to reach with one person.

This is not a promise that it stays this way. As soon as there is more than one maintainer,
disagreement on a structural decision is resolved by the maintainers **discussing it in the
open**, on the PR or issue where the ADR is proposed — never in private — and, if that doesn't
converge, by the majority of active maintainers. A tie is broken by whoever has been a maintainer
longest. This is the same escalation path most small open-source projects converge on once they
outgrow "the one person decides," written down now instead of invented under pressure later.

## Adding a maintainer

Proposed by an existing maintainer, in the open, naming the track record that earned it (see
[MAINTAINERS.md](MAINTAINERS.md#becoming-a-maintainer)). Accepted the same way any structural
change is: no objection from an existing maintainer within a reasonable review window. The new
maintainer is added to `MAINTAINERS.md` and — once area ownership stops being "everything, one
person" — to `.github/CODEOWNERS` for the areas they own.

## Removing a maintainer

For inactivity (no review, no commits, unresponsive for an extended period) or for a conduct
violation under [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md). Proposed in the open by another
maintainer, with the same objection window as adding one. A maintainer can also step down
unilaterally at any time — see [MAINTAINERS.md](MAINTAINERS.md#stepping-down).

## The escape valve

Delonix Runtime is Apache-2.0 (see [LICENSE](LICENSE)). If governance here ever stops serving the
people using this engine — a maintainer disappears, a decision the community can't live with gets
made and doesn't get reversed — the license is what guarantees a fork is always possible, by
anyone, with the full history intact. Good governance should make that path unnecessary; the
license is what makes it never *impossible*.
