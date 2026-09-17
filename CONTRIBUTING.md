# Contributing to Delonix Runtime

Thanks for considering a contribution. This is a systems project (namespaces, cgroups, nftables,
raw `clone()`/`unshare()`) — small mistakes here can have real security or stability consequences,
so we lean on tests, live validation, and careful review more than most projects.

**Contributor handbook:** <https://angolardevops.github.io/delonix-runtime/handbook/> (English,
Português de Angola, Français; source in [docs/dev/README.md](docs/dev/README.md)) — host requirements, building,
every CI gate as a local command, the architecture, the crates, and the contribution workflow. This
file is the short version.

## Before you start

- Skim [README.rst](README.rst) for the shape of the project, and
  [docs/dev/05-architecture.md](docs/dev/05-architecture.md) and
  [docs/dev/06-crates.md](docs/dev/06-crates.md) for the layers and the crate layout. The engine
  runs containers and microVMs on one node, daemonless and rootless-first, and knows no consumer —
  read «Identidade e fronteira do motor» at the top of [AGENTS.md](AGENTS.md) before proposing a
  change to its boundaries.
- For anything non-trivial (a new command, a new manifest `Kind`, a change to the namespace/cgroup
  setup), open an issue first to discuss the approach before writing code. It saves everyone time.
- Check open issues and pull requests so you don't duplicate work already in flight.

## Development setup

```bash
git clone https://github.com/angolardevops/delonix-runtime.git
cd delonix-runtime
cargo build --workspace
cargo test --workspace
```

You need the pinned Rust toolchain (`rust-toolchain.toml`) and `protoc` on `PATH` — `delonix-cri`
compiles the CRI protobuf with `tonic-build` (`sudo apt install protobuf-compiler` on
Debian/Ubuntu). Running containers also needs a Linux host with cgroup v2, unprivileged user
namespaces, a subuid range and `slirp4netns`/`nftables`/`iproute2`/`uidmap`; see
[docs/dev/01-environment.md](docs/dev/01-environment.md) for the full list and the host traps
(AppArmor on recent Ubuntu, cgroup delegation over SSH).

Always test the binary you built (`./target/debug/delonix`), not a `delonix` installed on your
`PATH`. Before running anything beyond `--help`, isolate the engine's state by exporting **both**
`DELONIX_ROOT` and `DELONIX_NET_RUNTIME_DIR` to scratch directories — see
[docs/dev/02-build-and-test.md](docs/dev/02-build-and-test.md#isolating-the-engines-state).

## Before opening a PR

CI runs more than build, clippy, fmt and test: a language ratchet, the architecture fitness gate,
the node contract gate, the version gate, the CLI surface and documentation gates, `cargo-deny` and
the generated site. Each one, with the command to run it locally, is listed in
[docs/dev/02-build-and-test.md](docs/dev/02-build-and-test.md#the-gates-ci-runs). Zero clippy
warnings is enforced, not a suggestion. How to work (one worktree per task, version alignment,
English-only code, when to write an ADR) is in
[docs/dev/10-contributing-workflow.md](docs/dev/10-contributing-workflow.md).

**If you touch runtime/namespace/cgroup code**, unit tests alone don't prove much — validate live
against a real container on a Linux host before opening the PR, and say what you tested in the PR
description (command run, expected vs. actual behavior).

**If you add or change a CLI command:**
- New user-facing strings are authored in English in the source and wrapped in
  `po::t(...)`/`po::tf(...)` (see `bins/delonix-runtime-bin/src/cmd/po.rs`); the Portuguese
  translation goes in `bins/delonix-runtime-bin/data/pt.po`, never inline in the code. Portuguese
  left in code is counted by `scripts/lang_ratchet.py` in CI — a string that shows up in Portuguese
  when running with the default (English) language is a bug.
- If the command has multiple entry points that should behave the same way (a common pattern in
  this codebase — see `delonix vm`/`delonix image vm`/`delonix image --vm`), wire all of them, not
  just the first one you find.
- Write a unit test for any new pure function (parsers, validators, URL builders) — this codebase
  has a strong track record of catching real bugs this way.

**If you touch anything that crosses a privilege or namespace boundary** (userns mapping, the
holder netns, the control socket, `setns`/`unshare`, path handling for anything driven by
user/manifest input) — flag this explicitly in the PR description. These get extra scrutiny; see
[SECURITY.md](SECURITY.md) if you're not sure whether something is a vulnerability worth
disclosing privately instead of a normal PR.

## Style

- `cargo fmt` defaults, no custom config — just run it.
- No new comments unless they explain a non-obvious *why* (a hidden constraint, a workaround for a
  specific bug, an invariant that isn't clear from the code). Comments that restate what the code
  does are removed in review.
- Don't add abstractions, config flags, or error handling for cases that can't happen. This
  codebase prefers direct, readable code over defensive scaffolding — see the existing modules in
  `bins/delonix-runtime-bin/src/cmd/` for the house style.

## Commit messages and PRs

- Keep commits focused — one logical change per commit is easier to review and to `git bisect`
  later.
- Describe *why*, not just *what* — the diff already shows what changed.
- Reference the issue you're addressing if there is one.

## Releases

Releases are cut by maintainers following a fixed pipeline (version bump → tag → CI build →
publish → live validation against the published binary). You don't need to worry about this as a
contributor — just make sure your change is documented in your PR description so it can be folded
into the next release's notes.

## Reporting bugs

Use [GitHub Issues](https://github.com/angolardevops/delonix-runtime/issues). Include:
- `delonix --version`
- The exact command you ran and what you expected vs. what happened
- Whether you're running rootless or as root, and your distro/kernel version

For anything that looks like a security vulnerability (privilege escalation, namespace escape,
command injection, path traversal), please **do not** open a public issue — see
[SECURITY.md](SECURITY.md) instead.

## License

By contributing, you agree that your contributions will be licensed under the Apache-2.0 license
that covers the rest of the project (see [LICENSE](LICENSE)).
