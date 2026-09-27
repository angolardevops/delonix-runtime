## What does this change do, and why?

<!-- The "why" matters more than the "what" here — the diff already shows what changed. -->

## How was this tested?

<!--
- `cargo build --workspace` / `clippy` / `fmt --check` / `test --workspace`: all clean?
- If this touches runtime/namespace/cgroup/network code: what did you run LIVE (not just unit
  tests) to confirm it actually works on a real host? Paste the command + output.
-->

## Checklist

- [ ] `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --locked -- -D warnings`
      and `cargo test --workspace --locked` are all clean
- [ ] The ratchets and structure gates pass: `python3 scripts/lang_ratchet.py` and
      `python3 scripts/arch_fitness.py`. If a count went DOWN, the baseline is lowered in this
      same PR (`lang_ratchet.py --update`); both fail on a drop that was not recorded
- [ ] If this touches `proto/`: `python3 scripts/contract_gate.py` passes (it needs `buf`;
      `docs/api/openapi.yaml` is generated, never edited by hand)
- [ ] If this changes the CLI or anything `docs/` is generated from: `python3 docs/gen.py` was
      run and its output is committed. If it touches `docs/dev/`: `python3 scripts/dev_docs.py
      --check` and `python3 scripts/dev_docs_site.py --check` pass
- [ ] If this touches a `scripts/*.py` gate: its `scripts/test_*.py` passes
- [ ] New user-facing strings are English in the source, wrapped in `po::t`/`po::tf`, with a
      Portuguese entry added to `bins/delonix-runtime-bin/data/pt.po` (not applicable if this PR
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
