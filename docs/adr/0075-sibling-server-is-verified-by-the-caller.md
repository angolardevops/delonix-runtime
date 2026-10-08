# ADR-0075: `exec_server` refuses a sibling it did not find beside itself, instead of trusting the `PATH`

- **Status:** Accepted (2026-10-08, by the owner). **Nothing is implemented yet**: `exec_server`
  still takes the `PATH` fallback in silence, and D1/D2 are separate work with their own PR.
  Accepting a decision is not the same as having it, and the measurement that motivated this ADR
  still reproduces against a `delonix` run from a build tree. Until D1 lands, the battery guard
  merged in `#725` measures the same hole from the outside — which protects this repo's own runs,
  not a user's node.
- **Date:** 2026-10-08
- **Deciders:** Walter Angolar
- **Relates to:** ADR-0040 D2.4 as amended (each server is its own binary and `delonix serve <x>` /
  `delonix mcp` `exec` it, so a user only needs to know `delonix`), the engine's identity section in
  `AGENTS.md` («the engine validates its own contract»), guardrail 6 of the ADR process (no silent
  failure), and the battery guard merged in `#725`, which measures the same hole from the outside.

## Context

`delonix` does not serve the CRI, the node API, the MCP or the management API itself. Each is its
own binary and `delonix` `exec`s it through `exec_server`
(`bins/delonix-runtime-bin/src/cmd/serve.rs`), at four call sites: `delonix-cri`, `delonix-mgmt`,
`delonix-node-api` and `delonix-mcp`.

`exec_server` resolves the sibling **beside the executable first, and falls back to the bare name**,
which the kernel then resolves through the `PATH`:

```rust
let beside = std::env::current_exe().ok()
    .and_then(|exe| exe.parent().map(|d| d.join(name)))
    .filter(|p| p.is_file());
let program = beside.unwrap_or_else(|| std::path::PathBuf::from(name));
```

That fallback is silent. When it fires, `delonix` runs a server from **a different installation than
the one being run**, and nothing says so.

**There is already a guard for exactly this, and it is on the wrong side of the boundary.**
`delonix_node::dispatch::check_version` exists for this case — its own comment says «a server left
on the `PATH` from an older install would otherwise serve with code the user never installed
alongside their `delonix`». `exec_server` sets `DELONIX_DISPATCH_VERSION`, and all four servers call
`check_version` in their `main`. But the **callee** runs that check, so a callee that predates the
mechanism never runs it.

Measured 2026-10-08 against `~/.local/bin/delonix-cri` dated 2026-09-07, with both roots and the
socket isolated:

| probe | result |
|---|---|
| `DELONIX_DISPATCH_VERSION` set to a version it is not | **started and served** (`timeout` rc=124, log `delonix-cri starting`) |
| `strings` for `DELONIX_DISPATCH_VERSION` | **absent** — the binary predates the mechanism |
| `--version` | **started and served** instead of printing (rc=124) |

A check implemented in the callee fails in the one case it exists for: the older the sibling, the
less it protects. That is the inverse of what a guard must do, and it is a silent failure, which
guardrail 6 forbids.

**What it costs in practice, measured.** On 2026-10-08 a full battery run against a tree with only
`delonix` built gave **FAIL=3** in the ADR-0074 D1 section. The engine was correct; the run served
the 2026-09-07 `delonix-cri`, a month older than the refusal those checks assert, so the code under
test never ran. It took a complete run (~11 min) plus two falsified hypotheses to diagnose, and the
failures pointed at the engine the whole way. A second session independently lost a chaos-harness
run to the same class of mistake.

**The fallback is not load-bearing.** `scripts/install.sh` installs every binary into the same
`BIN_DIR` — `/usr/local/bin`, or `$HOME/.local/bin` with `--user`. So in any installation the
sibling *is* beside `delonix` and `beside` wins; the fallback only fires when `delonix` runs from a
directory that does not carry its siblings, which is precisely the situation where the `PATH` copy
belongs to some other install. The measured failure is the two halves of that: a `--user` install in
`~/.local/bin` and a `delonix` run from a build tree.

## Decision

**D1. `exec_server` refuses when the sibling is not beside the executable.** The resolution keeps
its first step and loses the silent second one: if `current_exe().parent()/<name>` is not a file,
`delonix` returns `Error::Unavailable` naming the server, the directory it looked in, and the
install hint it already carries — instead of `exec`ing whatever the `PATH` resolves. A refusal the
user reads beats a server the user did not install.

**D2. An explicit opt-in restores the old behaviour.** `DELONIX_SERVER_FROM_PATH=1` allows the
fallback, and when it fires `delonix` writes one line to stderr naming the resolved path. This
follows the owner's doctrine recorded in ADR-0074 D3: a capability is never dropped because the safe
default is narrower — it moves behind an explicit opt-in, and the refusal says «not beside», never
«impossible».

**D3. The callee-side `check_version` stays, and stops being the only line.** It still catches the
case D1 cannot see — a sibling that *is* beside `delonix` but belongs to another release — for every
server new enough to run it. D1 covers the case it structurally cannot.

## Alternatives considered

**Probe the sibling's `--version` before `exec`ing it.** Rejected on measurement: an obsolete
sibling *starts the server* on `--version` (rc=124 above), so the caller would have to spawn it,
time it out and kill its process group on every `delonix serve`. It is also only guaranteed for half
the siblings — `version_flag.rs` asserts «prints and exits without opening a socket» for
`delonix-cri` and `delonix-mcp`, and there is no such test for `delonix-mgmt` or
`delonix-node-api`. A guard whose own probe can hang, and which is tested for two of four targets,
is not a guard.

**Warn on the fallback but still take it.** Rejected by guardrail 6: a warning on stderr is what the
battery already had in its header, and the measured outcome is that it did not stop a wrong
measurement. Fail-closed, with an opt-in, is the shape this repo uses elsewhere.

**Read the version out of the binary without executing it.** Rejected: parsing a version string out
of an ELF is a heuristic, and a heuristic that is wrong refuses a correct install.

**Do nothing — the battery guard of `#725` is enough.** Rejected because it protects the wrong
population. That guard runs in the battery, so it protects *us*; a user with an older
`delonix-cri` on the `PATH` still runs code they never installed, silently. The battery guard and
D1 are complementary: one measures from outside, the other refuses from inside.

## Consequences

Easier: a wrong sibling becomes a named refusal at the moment of `serve`, not N failures in whatever
the server then does wrong. The failure mode that cost two sessions a run each stops existing for
anybody running from a build tree.

Harder: a setup that today works by accident stops working until the user either puts the sibling
beside their `delonix` or sets `DELONIX_SERVER_FROM_PATH=1`. The honest description is that it is a
behaviour change for a configuration nobody documented — `scripts/install.sh` never produces it —
but it *is* a change, and someone depending on it will see a refusal where they had a server.

Debt assumed: one more environment variable, and the matrix of «beside / opt-in / refused» has to
stay tested. D2's stderr line is the kind of thing that rots silently if nothing asserts it.

Known limitation, stated rather than hidden: D1 does not make a *beside* sibling trustworthy. A
stale binary copied next to `delonix` is still served, and only D3 catches it — and only when that
binary is new enough to run `check_version`. Closing that needs a probe, and the probe is what this
ADR rejects on measurement.

## What was not measured

- The refusal is **not implemented**; this ADR is `Proposed` and records the decision, not a landed
  change. No code in `exec_server` has moved.
- Whether any real user depends on the `PATH` fallback. The claim here is only that
  `scripts/install.sh` does not create that layout, which was read in the script, not surveyed
  across installs.
- `delonix-mgmt` and `delonix-node-api` were not probed for the «`--version` starts the server»
  behaviour on an obsolete copy. The measurement above is on `delonix-cri` only; the absence of a
  `version_flag.rs` for those two is a fact about the tests, not a measurement of old binaries.
