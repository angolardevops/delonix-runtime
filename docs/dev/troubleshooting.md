# Troubleshooting

**Before you read:** [Preparing your environment](environment.md) (host traps) and [Clone, build and test](build-and-test.md#the-gates-ci-runs) (the gates, and isolating the engine's state).

This page is an index by **symptom**: the literal text a gate or a live run prints, what it
actually means, and the fix. It does not re-teach what [Preparing your
environment](environment.md) and [Clone, build and test](build-and-test.md) already cover in
depth — it points at the right section instead, so a fresh clone can go from "I am stuck" to "I
know which section to read" without reading either page end to end first. If a symptom is not
here, [Start here § When you are stuck](start-here.md#when-you-are-stuck) is the next place to
look — this page is a shortcut for the specific case where the *tool itself already told you
what is wrong*, in text you did not recognise yet.

## Quick index

| You saw | It is | Fixed in |
|---|---|---|
| `FAIL <name>: N (baseline M) — new debt entered` | `scripts/arch_fitness.py` | [Architecture ratchets](#a-debt-ratchet-moved-arch_fitnesspy) |
| `FALHA <kind>: N > M — entrou português novo.` | `scripts/lang_ratchet.py` | [The language ratchet](#new-portuguese-entered-lang_ratchetpy) |
| `<name> (<layer>) → <name> (<layer>): forbidden direction` | `scripts/arch_fitness.py` | [A dependency goes against the layer direction](#a-dependency-goes-against-the-layer-direction) |
| `<file>:<n>: names a consumer (…) — the engine knows none` | `scripts/arch_fitness.py` | [A consumer name leaked into the engine](#a-consumer-name-leaked-into-the-engine) |
| `FAIL <tag> is published but this commit does not contain it` | `scripts/version_gate.py` | [The branch predates the newest tag](#the-version-gate-refuses-your-branch) |
| `FAIL Cargo.toml says X, above Y, and docs/releases/vX.md does not exist` | `scripts/version_gate.py` | [A version bump with no release commit](#the-version-gate-refuses-your-branch) |
| `FAIL  buf format` / `buf lint` / `buf breaking against …` / `has no google.api.http mapping` / `openapi.yaml is not the generated one` | `scripts/contract_gate.py` | [The node contract gate](#the-node-contract-gate) |
| `unshare()` fails, `EPERM` | AppArmor + user namespaces | [Preparing your environment § AppArmor](environment.md#ubuntu-2310-apparmor-blocks-user-namespaces-for-your-dev-binary) |
| `-m`/`--cpus`/`--cpu-weight` refused, exit `69` | cgroup delegation | [Preparing your environment § cgroup delegation](environment.md#cgroup-delegation-some-limits-are-refused-others-are-not-enforced) |
| `path must be shorter than SUN_LEN` | `DELONIX_NET_RUNTIME_DIR` too long | [Clone, build and test § Isolating the engine's state](build-and-test.md#isolating-the-engines-state) |
| A gate fails against a diff you did not write, or a build finishes suspiciously fast | a stale or shared `CARGO_TARGET_DIR` | [A shared or stale build cache](#a-shared-or-stale-build-cache) |
| The binary answers with an old version or a command that does not exist | a stale `delonix` on `PATH` | [Preparing your environment § A stale PATH](environment.md#a-stale-delonix-on-your-path) |

## CI gates with a reason in the message

Every gate names what to fix — the table below is only here so you recognise the *shape* of the
message before reading it closely. Run the local command from [Clone, build and test § The gates
CI runs](build-and-test.md#the-gates-ci-runs) to reproduce any of these without waiting for CI.

### New Portuguese entered (`lang_ratchet.py`)

```
FALHA identifiers: 1051 > 1050 — entrou português novo.
       `python3 scripts/lang_ratchet.py --list --only identifiers` mostra onde.
```

(The gate's own messages are in Portuguese — LANG-01 is about the *code*, not this script; see
[Contribution workflow § Language](contributing-workflow.md#language-english-in-the-code-lang-01).)
The count of Portuguese identifiers, comments or user-facing messages rose. `--list --only
<kind>` names the new lines. If you *translated* something and the count instead **fell** without
you lowering the baseline, the message is the mirror image ("traduziste, mas não baixaste a linha
de base") — run `python3 scripts/lang_ratchet.py --update` and commit
`scripts/lang_baseline.json` in the same commit as the translation.

### A debt ratchet moved (`arch_fitness.py`)

```
FAIL  self_exec_sites: 6 (baseline 5) — new debt entered
```

One of the five tracked numbers (`self_exec_sites`, `library_prints`, `env_writes`,
`shared_error_imports`, `raw_error_variant_matches`) rose. `python3 scripts/arch_fitness.py
--list` prints every site each one counts, with a short explanation of what the count means next
to it — see [Contribution workflow § Architecture rules the gates
enforce](contributing-workflow.md#architecture-rules-the-gates-enforce) for what each one is. As
with the language ratchet, the same message shape appears when the count **falls** without the
baseline moving with it in the same commit ("debt was paid; lower the baseline … `--update`").

### A dependency goes against the layer direction

```
FAIL  delonix-oci (adapters) → delonix-cri (interfaces): forbidden direction
```

A crate imported another crate from a layer it is not allowed to depend on — see [Architecture §
Layers and the allowed direction](architecture.md#layers-and-the-allowed-direction) for the table
of who may depend on whom. Either the dependency is wrong (most of the time — an adapter has no
business depending on an interface), or the change genuinely needs a declared, phased exception
in `EXCEPTIONS` inside `scripts/arch_fitness.py`, which the gate itself refuses to accept without
a phase that removes it and a reason.

### A consumer name leaked into the engine

```
FAIL  crates/adapters/delonix-oci/src/registry.rs:42: names a consumer ('SomeControlPlaneName') — the engine knows none
```

The engine's canonical boundary — *«the engine knows no consumer»*, the section at the top of
[`AGENTS.md`](../../AGENTS.md) — is enforced by grep, not by review alone. This fires on the name
of any platform, control plane, console or agent that uses the engine, in code **or comments**,
anywhere under `crates/`, `bins/` or `proto/`. Generalise the requirement into the capability it
actually is, in the engine's own vocabulary; history that needs the external name belongs in
`docs/`, never in code.

### The version gate refuses your branch

```
FAIL  v1.4.2 is published but this commit does not contain it (newest contained: v1.4.0) —
      merge origin/main first; merging a branch that predates a release undoes what it shipped
```

Your branch started before a release that has since shipped. `git fetch --tags origin && git
merge origin/main` (this repository merges, it does not rebase feature branches onto releases —
see [Contribution workflow § One worktree per task](contributing-workflow.md#one-worktree-per-task)
for why a linear history is still expected from your own commits).

```
FAIL  Cargo.toml says 1.5.0, above 1.4.2, and docs/releases/v1.5.0.md does not exist —
      a bump belongs only to the release commit
```

You bumped `version` in `Cargo.toml`. Do not — that is done only in the release commit, together
with the release notes file. Revert the bump; see [Releases and stability § The version
gate](releases-and-stability.md#the-version-gate).

### The node contract gate

`scripts/contract_gate.py` wraps five independent checks, and each prints its own `FAIL` line —
running it locally is the fastest way to see which of the five is yours:

```
FAIL  buf format — run `buf format -w proto`
FAIL  buf lint
FAIL  buf breaking against v1.4.0
<buf's own stdout/stderr follows, naming the field or RPC that changed incompatibly>
FAIL  node.proto: SomeRpc has no google.api.http mapping
FAIL  docs/api/openapi.yaml is not the generated one — run `python3 scripts/contract_gate.py --update` and commit it
```

It needs `protoc`, `buf` (pinned at v1.73.0) and `protoc-gen-openapi` (pinned at v0.7.1) on
`PATH`, and the git tags fetched — a missing tool fails with the more familiar "command not
found", but a missing tag makes the `buf breaking` check print `ok` while explicitly saying there
is no baseline yet to compare against, rather than silently skipping it. A genuine breaking
change to `proto/delonix/node/v1` needs an ADR first, same as any change to a stable node
contract — see [Contribution workflow § When to write an ADR](contributing-workflow.md#when-to-write-an-adr).
If what actually changed is generator output (a new field, a new RPC), `python3
scripts/contract_gate.py --update` regenerates `docs/api/openapi.yaml`; commit it in the same
commit as the `.proto` change.

## A shared or stale build cache

[Clone, build and test § Build](build-and-test.md#build) already names the trade-off: pointing
several worktrees at one shared `CARGO_TARGET_DIR` saves disk, but two builds running against it
at the same time wait on each other and **can invalidate each other's artefacts**. The symptom is
specific and easy to misread as a real failure: a gate (particularly `test` or `clippy`) fails
against code that looks unrelated to your change, or a build finishes suspiciously fast and then
the binary it produced behaves like an older version — including a local pre-commit or pre-push
hook that reuses a shared target directory across sessions and links against whatever object
files happen to be sitting there from a different, concurrent build.

This is not a bug in the gate: it is genuinely building the wrong thing. Rule out a stale cache
before debugging the "failure" itself:

```bash
cargo clean -p delonix-runtime-bin   # or the crate the failure points at
cargo build -p delonix-runtime-bin   # rebuild clean, then re-run the gate that failed
```

If you routinely work from several worktrees at once, giving each one its own `CARGO_TARGET_DIR`
(unset the shared one, or export a worktree-local path) removes the class of symptom entirely, at
the cost of a slower first build in each — the same trade-off [Clone, build and test](build-and-test.md#build)
already states.

---

**Next:** [Coding conventions](coding-conventions.md) — how code in this repository must be written, each rule tagged with the gate or decision behind it.
