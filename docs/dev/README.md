# Delonix Runtime — Contributor Handbook

This handbook is for people who want to **change the engine**: you cloned the repository today and
want to send a first pull request without breaking your host or the engine. If you only want to
*use* Delonix, start with the [README](../../README.rst) and the
[user documentation site](https://angolardevops.github.io/delonix-runtime/) instead.

<!-- dev-docs:begin crate-count -->
The workspace has **22 crates** and ships **4 binaries** (`delonix`, `delonix-cri`, `delonix-mcp`, `delonix-mgmt`).
<!-- dev-docs:end crate-count -->

## What the engine is — and what it is not

Delonix Runtime is an execution abstraction for **one node**: it runs **containers and microVMs**
and manages the networking and storage they need. It is declarative (its own Kinds, grouped by
`apiVersion` — see `delonix api-resources`), and it talks to providers (the Linux kernel, libvirt,
Cloud Hypervisor, Proxmox VE, the Kubernetes CRI) only through ports, never through
`if provider == …` branches spread across the code.

Three principles shape almost every review comment you will get:

- **Cloud native** — plan / apply / drift, API-first (the CLI, the node API, the CRI and the MCP
  server expose the same operations), observable through open standards.
- **Daemonless** — no resident process by default. What must persist belongs to systemd or to a
  per-workload process with a clear owner. A new daemon needs an ADR.
- **Rootless-first** — the normal path runs without root; privilege is an explicit opt-in.

And one boundary that is enforced by a CI gate: **the engine knows no consumer.** It does not know
who calls it, and it has no notion of tenant, account, plan or billing. A requirement coming from a
consumer enters as a generic engine capability, or it does not enter. The canonical text is the
section *«Identidade e fronteira do motor»* at the top of [`AGENTS.md`](../../AGENTS.md).

## Two halves: generated facts and narrative

Pages in this handbook mix two kinds of content:

- **Facts** — which crates exist, their layer, who depends on whom, the binaries, the pinned
  toolchain, the CI jobs. They live between `<!-- dev-docs:begin <key> -->` and
  `<!-- dev-docs:end <key> -->` markers and are **generated** by `python3 scripts/dev_docs.py`
  from `Cargo.toml`, `scripts/arch_fitness.py`, `rust-toolchain.toml` and
  `.github/workflows/ci.yml`. Never edit them by hand — CI runs `dev_docs.py --check` and fails.
  If a fact is wrong, fix the source or the generator.
- **Narrative** — why things are the way they are, how flows work, how to contribute. It is
  written by hand and reviewed after each release. See [11 — Publishing the docs](11-publishing-docs.md).

## Reading paths

| If you want to… | Read, in order |
|---|---|
| **Start from zero, with no one to ask** | [00](00-start-here.md) (keep [14](14-glossary.md) open) → the path below that matches your change |
| **Send a first PR** (a CLI fix, a docs fix, a small feature) | [00](00-start-here.md) → [01](01-environment.md) → [02](02-build-and-test.md) (keep [15](15-environment-variables.md) at hand) → [10](10-contributing-workflow.md) → [12](12-coding-conventions.md) → [06](06-crates.md) for the crate you touch |
| **Understand the engine in depth** | [03](03-rust-primer.md) → [04](04-cloud-native-primer.md) → [13](13-cloud-native-standards.md) → [05](05-architecture.md) → [06](06-crates.md) → [07](07-system-design-interview.md) |
| **Work on VMs or VM images** | [01](01-environment.md) → [02](02-build-and-test.md) → [08](08-delonixfile-and-vmfile.md) → [09](09-microvm-setup.md) → the `delonix-vm` section of [06](06-crates.md) → the VM tuning section of [15](15-environment-variables.md) |
| **Change how documentation is produced** | [11](11-publishing-docs.md) |
| **Configure, isolate or tune a run** (state roots, logging, escape hatches, providers) | [02 — Isolating the engine's state](02-build-and-test.md#isolating-the-engines-state) → [15](15-environment-variables.md) |

## Pages

| # | Page | What it answers |
|---|---|---|
| 00 | [Start here](00-start-here.md) | Day 0 setup check, your first contribution end to end, where a change goes, the rules and their sources, what to do when stuck |
| 01 | [Preparing your environment](01-environment.md) | What the kernel and host need, the pinned toolchain, and the host traps that look like engine bugs |
| 02 | [Clone, build and test](02-build-and-test.md) | Building, running tests, every CI gate as a local command, E2E and chaos with isolation |
| 03 | [Rust primer](03-rust-primer.md) | The Rust this codebase actually uses |
| 04 | [Cloud native primer](04-cloud-native-primer.md) | Namespaces, cgroups v2, OCI, CRI, CNI, nftables, KVM — and where each appears in the engine |
| 05 | [Architecture](05-architecture.md) | Layers, the crate graph, control and data paths, state on disk |
| 06 | [The crates](06-crates.md) | One block per crate: responsibility, main types, where to start reading |
| 07 | [System Design Interview](07-system-design-interview.md) | The engine designed as an interview answer, then compared with what was built |
| 08 | [Delonixfile and VMfile](08-delonixfile-and-vmfile.md) | The build file grammars and how they differ from a Dockerfile |
| 09 | [Building microVMs](09-microvm-setup.md) | KVM, Cloud Hypervisor and firmware, libvirt, VM images |
| 10 | [Contribution workflow](10-contributing-workflow.md) | Worktrees, versions, language rule, architecture rules, ADRs, commits and PRs |
| 11 | [Publishing the documentation](11-publishing-docs.md) | How the site and this handbook are generated, gated and published |
| 12 | [Coding conventions](12-coding-conventions.md) | How code in this repository is written, and the checklist reviewers apply |
| 13 | [Cloud native standards](13-cloud-native-standards.md) | The cloud-native standards a change is measured against |
| 14 | [Glossary](14-glossary.md) | The engine and cloud-native terms you meet here, with their Delonix meaning and where to read more |
| 15 | [Environment variables](15-environment-variables.md) | Every `DELONIX_*` variable the code reads: who reads it, what it changes, its default, and which ones lower a boundary |

Other references you will be pointed to: [`ARCHITECTURE.md`](../../ARCHITECTURE.md) (C4 diagrams),
[`docs/adr/`](../adr/README.md) (architecture decisions), [`SECURITY.md`](../../SECURITY.md)
(private vulnerability reports) and [`CONTRIBUTING.md`](../../CONTRIBUTING.md) (the short front door).
