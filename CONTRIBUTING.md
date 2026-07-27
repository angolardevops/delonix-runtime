# Contributing to Delonix Runtime

Thanks for considering a contribution. This is a systems project (namespaces, cgroups, nftables,
raw `clone()`/`unshare()`) — small mistakes here can have real security or stability consequences,
so we lean on tests, live validation, and careful review more than most projects.

## Before you start

- Skim [README.rst](README.rst) for the shape of the project, and
  [docs/arquitectura.html](https://angolardevops.github.io/delonix-runtime/arquitectura.html) for
  the crate layout.
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

`delonix-cri` (the Kubernetes CRI server) uses `tonic-build`, which needs `protoc` on `PATH`:

```bash
# Debian/Ubuntu
sudo apt install protobuf-compiler
# or download a release from https://github.com/protocolbuffers/protobuf/releases
```

Build and try the CLI directly:

```bash
cargo build -p delonix-runtime-bin
./target/debug/delonix container run -d --name web -p 8080:80 nginx
```

Most of the runtime needs `slirp4netns`/`nftables`/`uidmap` on the host to actually create
containers — see the [install.sh](install.sh) script for what a fully-functional host needs, or
run `cargo test` for pure-logic coverage that doesn't touch the kernel.

## Before opening a PR

Run the full local gate — this is exactly what CI checks:

```bash
cargo build --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
cargo test --workspace
```

All four must be clean. Zero clippy warnings is enforced, not a suggestion.

**If you touch runtime/namespace/cgroup code**, unit tests alone don't prove much — validate live
against a real container on a Linux host before opening the PR, and say what you tested in the PR
description (command run, expected vs. actual behavior).

**If you add or change a CLI command:**
- New user-facing strings are authored in English in the source and wrapped in
  `po::t(...)`/`po::tf(...)` (see `crates/delonix-runtime-bin/src/cmd/po.rs`); the Portuguese
  translation goes in `crates/delonix-runtime-bin/data/pt.po`, never inline in the code. This is
  enforced by review, not by a lint — a string that shows up in Portuguese when running with the
  default (English) language is a bug.
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
  `crates/delonix-runtime-bin/src/cmd/` for the house style.

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
