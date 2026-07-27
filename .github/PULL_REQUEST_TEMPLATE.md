## What does this change do, and why?

<!-- The "why" matters more than the "what" here — the diff already shows what changed. -->

## How was this tested?

<!--
- `cargo build --workspace` / `clippy` / `fmt --check` / `test --workspace`: all clean?
- If this touches runtime/namespace/cgroup/network code: what did you run LIVE (not just unit
  tests) to confirm it actually works on a real host? Paste the command + output.
-->

## Checklist

- [ ] `cargo build --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`,
      `cargo fmt --all --check`, and `cargo test --workspace` are all clean
- [ ] New user-facing strings are English in the source, wrapped in `po::t`/`po::tf`, with a
      Portuguese entry added to `crates/delonix-runtime-bin/data/pt.po` (not applicable if this PR
      doesn't touch CLI output)
- [ ] If this adds/changes a command with multiple entry points (see CONTRIBUTING.md), all of them
      are wired consistently
- [ ] New pure functions (parsers, validators, etc.) have unit tests
- [ ] If this crosses a privilege/namespace boundary, I've called that out explicitly below

## Does this cross a privilege or namespace boundary?

<!--
userns mapping, the holder netns, the control socket, setns/unshare, or path handling driven by
user/manifest input. If yes, describe the boundary and why the change is safe. If you're not sure
whether this needs security review, say so — better to ask.
-->
