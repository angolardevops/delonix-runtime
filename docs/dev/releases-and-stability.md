# Releases and stability

**Before you read:** [Contribution workflow](contributing-workflow.md#version-alignment) (the version gate) and [Publishing the documentation](publishing-docs.md) (what a release regenerates).

This page is about the other half of a release: what the `version` in `Cargo.toml` promises,
what happens when a `v*` tag is pushed, and what the engine guarantees will not break without a
major version. [Publishing the documentation](publishing-docs.md) already covers the docs side of
a release (what regenerates, what CI checks); this page covers the version number itself and the
CLI/manifest contract it backs.

## The version gate

`scripts/version_gate.py` (CI job `version`, `fetch-depth: 0` because it needs every tag) allows
the workspace `version` in the root `Cargo.toml` to be exactly one of two things:

1. **Equal to the newest tag this commit contains.** Ordinary work between releases. Two builds
   with the same version number are told apart by `delonix --version`, which prints the distance
   from that tag: `commit: <hash> (+N commits since vX.Y.Z)`.
2. **Greater than the newest contained tag, only in the release commit** — and then
   `docs/releases/v<version>.md` must exist, because the release workflow publishes it as the
   GitHub Release body.

Everything else fails, each with a reason tied to a real failure mode:

| What the gate sees | Why it fails |
|---|---|
| A newer tag exists that this commit does not contain | The branch started before that release; merging it as-is undoes what the release already shipped. Merge `origin/main` in first. |
| `Cargo.toml` is lower than the newest contained tag | An older version number would be published over a newer one. |
| `Cargo.toml` is higher than the newest contained tag, with no `docs/releases/v<version>.md` | A bump with no matching tag — sends the `delonix-cri` download (which is resolved from the running binary's own version, see `vmimage.rs`) to a release that will never exist. |

The version is not decoration: besides the `delonix-cri` download, it is the Docker API's
`ServerVersion`, the `delonix_version` recorded in every resource backup, and what the release
workflow (below) checks the built binary against before publishing anything.

Do not bump the version in a feature PR, and do not use a `-dev` suffix — the gate's first rule
already covers ordinary work, and a `-dev` suffix would point the CRI download at a release that
does not exist.

## What publishing a release does

Pushing the tag publishes **nothing**. Since 2026-09-23 the release workflow is
`workflow_dispatch`-only: the tag and the release are two separate decisions, so a tag that
reaches the remote by accident (`git push --tags`, `push.followTags=true`) can no longer put a
binary in front of the world. A tag with no release is a normal intermediate state here.

```bash
git tag -a v4.4.0 -m "v4.4.0" && git push origin v4.4.0   # cuts the version
gh workflow run release.yml -f tag=v4.4.0                 # publishes it
```

A `guard` job runs first and costs seconds: it refuses a `tag` that is not shaped `vX.Y.Z`, one
that does not exist on the remote, and one that already has a release — the input picks the commit
to build and names the release, so an unchecked `tag: main` would have published a release called
"main". Only then, in one job:

1. Builds `delonix`, `delonix-cri`, `delonix-mcp` and `delonix-mgmt` twice — once generic
   x86-64, once with `-C target-cpu=x86-64-v3` (AVX2/BMI2/FMA) — on `ubuntu-22.04` specifically,
   so the glibc baseline (2.35) stays compatible with RHEL 9 and Debian 12, not just the newest
   Ubuntu. `scripts/install.sh` picks the `-v3` build automatically when the host CPU supports it.
   A separate `build-arm64` job builds the same four binaries natively on an aarch64 runner (one
   per component, no `-v3` variant), and they are published as `<name>-aarch64-linux` under the same
   `SHA256SUMS`. `install.sh` does not install them yet. The job only runs at release time, so its
   first execution was the v4.2.0 release itself; CI runs the test suite natively on arm64 in the
   `test (arm64)` job on every PR.
2. **Regenerates the user site against this exact release build and fails if `docs/` differs.**
   This gate exists because it once did not: a site gap shipped live in v0.48.0, hiding a new
   command for hours while a parallel CI job was already red about it — the two workflows just
   were not looking at each other. Running `docs/gen.py` here, before publishing, closes that.
3. Builds a checksummed `SHA256SUMS`, an SPDX 2.3 SBOM (`scripts/sbom.py`, from `Cargo.lock`,
   itself hashed into `SHA256SUMS`), and — when the `MINISIGN_SECRET_KEY` secret is configured —
   a minisign signature over `SHA256SUMS`. `SHA256SUMS` alone only proves transfer integrity (it
   comes from the same URL as the binary); the signature proves the release came from this
   project, since `install.sh` carries the public key embedded and refuses to install an
   unsigned release without `--insecure-skip-signature`. Failing to sign when a key **is**
   configured is a hard error — it would break every installer's signature check at once.
4. Verifies its own binary reports the tag's version (`delonix --version | grep <tag>`) before
   publishing anything — the same class of check `version_gate.py` runs earlier, against the
   artefact that is actually about to ship.
5. Attaches SLSA build provenance (`actions/attest-build-provenance`, GitHub's own action, kept
   separate from minisign on purpose: provenance proves *where and from which commit* something
   was built, to someone who does not trust the project up front; minisign proves the release is
   *this project's*, to someone who already trusts its embedded public key).
6. Publishes the GitHub Release, with `docs/releases/<tag>.md` as its notes when that file
   exists, or `--generate-notes` otherwise.
7. Checks out `main` and regenerates `docs/RELEASES.md` (`scripts/gen-releases.sh`) and the
   handbook's generated facts (`scripts/dev_docs.py`) and, separately, the handbook **site**
   (`scripts/dev_docs_site.py`) — committing whichever changed, `[skip ci]`, so the docs never
   trail a release by more than this one commit. A generator failure here is a loud warning, not
   a failed release: the release itself does not depend on it.

The **narrative** half of the handbook — the prose on these pages — is not regenerated by CI.
After a release is published, a maintainer-run review reads what changed since the previous tag
(commits, release notes) and updates only the pages that change affects, exactly as described in
[Publishing the documentation § What happens at release time](publishing-docs.md#what-happens-at-release-time).

## What is stable, and where that promise lives

`docs/cli-stability.md` is the actual contract, in the same repository, read by `delonix explain`
and generated pages alike — this section only orients you to it, since duplicating its content
here would give it a second copy to fall out of step with the code. It has applied since v0.42.3
and, as of v1.0.0, reads as the project's real semver promise rather than a within-`0.x` note.

**Stable — does not break without a major:**

- The container/image lifecycle verbs (`container run`, `ps`, `stop`, `exec`, …) and image
  verbs, with Docker/Podman's own names and argument order, and the specific short/long flags
  `docs/cli-stability.md` lists for `run`/`exec`.
- Exit codes (`0` success, `4` not found, `5` conflict, `69` missing host capability, `124`
  timeout, …) and the `DX-CDNN` dictionary number each failure carries (ADR-0043) — the number
  identifies *which* failure and never changes meaning or gets reused; `delonix explain DX-4501`
  looks one up.
- `-o json` on every listing command: fields can be added, never removed or retyped (ADR-0005).
- The **manifest schema** for Kinds with a typed spec (`Container`, `Pod`, `Volume`, `Network`,
  and the others `delonix manifest schema` lists): a field is never removed, retyped or
  repurposed; a new field is always optional with a default that preserves old behaviour; a
  renamed field keeps the old spelling as an alias; `apiVersion: delonix.io/v1` keeps loading
  even after the per-domain groups (`compute.delonix.io/v1alpha1`, …) became canonical. This is
  the promise that matters most in practice — it protects what people put in git and review in a
  PR, not just what they type at a prompt.

**Not stable — can change in any version:** `serve cri`/`serve api`/`serve docker-api` (the local
management API in particular has no published contract and is explicitly not something to
automate against — see [The crates § `delonix-mgmt`](crates.md#delonix-mgmt) and ADR-0040/0041);
the `cluster`/`vm`/`pod`/`workload`/`net` imperative surfaces (their manifest **schema**, where one
exists, is covered above — only the verbs and flags around it are not); `compose`; `backup`;
`mcp`; `system`/`dashboard`/`completion`/`init`/`man`/`config`/`explain`; the on-disk state format
under `$DELONIX_ROOT`; and `stack history`/`stack rollback` (ADR-0019 — nothing reads that
history to decide what exists, so losing it changes nothing the reconciler does).

## How a breaking change is made, when it has to happen

The precedent, already applied more than once (the v0.30.0 CLI reorganisation, the v2.0.0
`image list`→`image ls` reversal): **a clean cut, no compatibility alias.** The old spelling fails
with `unrecognized subcommand`, loudly, in every version from the break onward — never a silent
alias that quietly changes behaviour later. `docs/cli-stability.md § Como uma quebra é feita`
records a real lesson from doing this: a rename can leave an *internal* caller behind (the CRI
server itself kept invoking a removed `delonix netns attach` for months after the v0.30.0
reorganisation, breaking rootless pod creation) — grepping the whole workspace for the old
spelling, not just the docs and the tests, is part of making the cut.

A breaking change to something this page or `docs/cli-stability.md` marks stable needs an ADR
first ([Contribution workflow § When to write an ADR](contributing-workflow.md#when-to-write-an-adr)),
because it moves a structural boundary by definition — the same reasoning that applies to a new
backend or a new privilege boundary applies here too.

---

**Next:** [Publishing the documentation](publishing-docs.md) — how the site and this handbook are generated, gated and published, and what your PR must regenerate.
