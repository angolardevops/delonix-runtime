# Delonix Runtime — Contributor Handbook

This handbook is for people who want to **change the engine**: you cloned the repository today and
want to send a first pull request without breaking your host or the engine. After this page you
will know how the handbook is sequenced and which pages to read, in which order, for your role. If
you only want to
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
  written by hand and reviewed after each release. See [Publishing the docs](publishing-docs.md).

## How this handbook is organised

The pages form **one course**, read top to bottom. Each page opens with a **Before you read** line
naming the earlier pages it assumes, and ends with a **Next** line pointing to the page that builds
on it. The number shown next to a page in the site's sidebar is its position in this order. Section
numbers *inside* a page (for example §3.8 in the Rust primer or 13.4 in the standards page) are
local labels kept stable for links; they are not page positions.

The course is grouped in eight parts:

| Part | Pages | What you get from it |
|---|---|---|
| **Get started** | [Start here](start-here.md) · [IaaS and cloud native](iaas-and-cloud-native.md) | A working checkout, a first contribution path, and the mental model of where a node engine sits in a cloud |
| **Foundations** | [Linux foundations](linux-foundations.md) · [Cloud native primer](cloud-native-primer.md) · [Rust primer](rust-primer.md) | The kernel primitives hands-on, how the engine uses each one, and the Rust this code is written in |
| **Set up & build** | [Preparing your environment](environment.md) · [Clone, build and test](build-and-test.md) | A host that can run the live paths, and every CI gate as a local command |
| **Architecture** | [Project structure](project-structure.md) · [Architecture](architecture.md) · [The crates](crates.md) · [System Design Interview](system-design-interview.md) | Where things are, why they are split that way, what each crate owns, and the reasoning behind the design |
| **Images & microVMs** | [Delonixfile and VMfile](delonixfile-and-vmfile.md) · [Building microVMs](microvm-setup.md) | The two build grammars, and VMs from host prerequisites to boot |
| **Operate & debug** | [Troubleshooting](troubleshooting.md) · [How names reach `/etc/hosts`](service-names-and-hosts.md) | A symptom-first index of what a gate or a live run prints, and the mechanism that publishes service names and route hosts on the operator's machine |
| **Contributing** | [Coding conventions](coding-conventions.md) · [Adding a Kind](adding-a-kind.md) · [Contribution workflow](contributing-workflow.md) · [Releases and stability](releases-and-stability.md) · [Publishing the documentation](publishing-docs.md) | How code must be written, how a declarative Kind is added, how a change is sent, what a release promises not to break, and how docs follow it |
| **Reference** | [Cloud native standards](cloud-native-standards.md) · [Environment variables](environment-variables.md) · [Glossary](glossary.md) | Pages you look things up in: conformance per standard, every `DELONIX_*` name, every term |

Concepts are **taught once**: a kernel primitive in [Linux foundations](linux-foundations.md), how
the engine uses it in [Cloud native primer](cloud-native-primer.md), and the standard it follows
with its conformance status in [Cloud native standards](cloud-native-standards.md). Where a
page mentions something taught elsewhere, it links there instead of repeating it.

## Reading paths by role

Nobody is expected to read all twenty-two pages before a first change. Pick the row that describes you
and read its pages in the order given; keep the [Glossary](glossary.md) open.

| Role | Read, in order — and why |
|---|---|
| **First PR, no time** | 1. [Start here](start-here.md) — Day 0 check and the eight steps of a first PR. 2. [Preparing your environment](environment.md#known-host-traps) — only *Known host traps*. 3. [Clone, build and test](build-and-test.md#the-gates-ci-runs) — the gates you must pass. 4. [The crates](crates.md) — only the section of the crate you touch. 5. [Contribution workflow](contributing-workflow.md) — how the PR is judged. |
| **DevOps engineer** (CI, packaging, installing, releases) | 1. [Start here](start-here.md) — the setup and the rules. 2. [Preparing your environment](environment.md) — what a host needs and the traps that look like engine bugs. 3. [Clone, build and test](build-and-test.md) — installing a build, every CI job as a local command, E2E and chaos. 4. [Project structure](project-structure.md) — what is generated, what CI checks it, what `release.yml` refreshes. 5. [Troubleshooting](troubleshooting.md) — recognising a gate failure by its message. 6. [Releases and stability](releases-and-stability.md) — the version gate, what a pushed tag does, what is stable. 7. [Publishing the documentation](publishing-docs.md) — what happens at release time. 8. [Environment variables](environment-variables.md) — every knob and which ones lower a boundary. |
| **Platform engineer** (building on the engine's interfaces) | 1. [IaaS and cloud native](iaas-and-cloud-native.md) — which layer the engine is and what it leaves to a control plane. 2. [Cloud native primer](cloud-native-primer.md#48-declarative-reconciliation) — Kinds and the three-way reconciler. 3. [Architecture](architecture.md) — the interfaces (CLI, CRI, management API, MCP, node contract) and the layers. 4. [The crates](crates.md) — `delonix-stack`, `delonix-cri`, `delonix-mgmt`, `delonix-mcp`. 5. [Adding a Kind](adding-a-kind.md) — the table and reconciler wiring a new Kind needs. 6. [System Design Interview](system-design-interview.md) — the API choices and their trade-offs. 7. [Cloud native standards](cloud-native-standards.md) — what is conformant, partial or absent, with dates. |
| **SRE** (operating nodes, diagnosing failures) | 1. [Linux foundations](linux-foundations.md) — answer "which namespace, which cgroup, who holds this fd" with a command. 2. [Preparing your environment](environment.md#diagnosing-the-host) — diagnosing a host and its traps. 3. [Troubleshooting](troubleshooting.md) — a symptom-first index for gate and runtime failures. 4. [Architecture](architecture.md#level-2-containers-executables-and-processes) — which processes exist at runtime, state on disk, known limitations. 5. [System Design Interview](system-design-interview.md#7-failure-modes-and-the-limits-of-one-node) — failure modes and the limits of one node. 6. [Coding conventions](coding-conventions.md#38-exit-codes-and-dx_-codes) — what an exit code means. 7. [Environment variables](environment-variables.md#observability) — logging, OTLP and the escape hatches. 8. [Cloud native standards](cloud-native-standards.md#1311-opentelemetry) — OpenTelemetry and Prometheus. |
| **Cloud developer** (Kinds, manifests, images, Compose/Docker compatibility) | 1. [IaaS and cloud native](iaas-and-cloud-native.md) — principles as they appear in the code. 2. [Cloud native primer](cloud-native-primer.md) — OCI images and declarative reconciliation. 3. [Clone, build and test](build-and-test.md) — build and run isolated. 4. [The crates](crates.md#delonix-stack) — `delonix-stack` and `delonix-oci`. 5. [Delonixfile and VMfile](delonixfile-and-vmfile.md) — the build grammars. 6. [Coding conventions](coding-conventions.md#36-kinds-api-groups-and-manifest-fields) — rules for Kinds and fields. 7. [Adding a Kind](adding-a-kind.md) — wiring a Kind into the reconciler end to end. 8. [Cloud native standards](cloud-native-standards.md#138-the-workload-api-own-kinds-and-the-node-contract) — own Kinds, Docker API and Compose subsets. |
| **Linux developer** (namespaces, cgroups, networking, VMs) | 1. [Linux foundations](linux-foundations.md) — the primitives hands-on. 2. [Cloud native primer](cloud-native-primer.md) — where each primitive lives in the code. 3. [Rust primer](rust-primer.md#34-unsafe-ffi-and-linux-syscalls) — `unsafe`, syscalls, `fork`/`clone` in threaded processes. 4. [Preparing your environment](environment.md) — AppArmor and cgroup delegation traps. 5. [Architecture](architecture.md) — the rootless network infrastructure and the two flows as sequences. 6. [The crates](crates.md#delonix-linux) — `delonix-linux`, `delonix-sdn`, `delonix-vm`. 7. [Building microVMs](microvm-setup.md) — KVM, Cloud Hypervisor, libvirt. 8. [Coding conventions](coding-conventions.md#7-unsafe-syscalls-and-processes) — the rules for `unsafe` and processes. |

Two narrower tasks have their own shortcut: **changing how documentation is produced** starts at
[Publishing the documentation](publishing-docs.md); **configuring or isolating a run** starts at
[Isolating the engine's state](build-and-test.md#isolating-the-engines-state) and then
[Environment variables](environment-variables.md).

## Pages

| Page | What it answers |
|---|---|
| [Start here](start-here.md) | Day 0 setup check, your first contribution end to end, where a change goes, the rules and their sources, what to do when stuck |
| [IaaS and cloud native](iaas-and-cloud-native.md) | What an IaaS is made of, which layer this engine is, what it leaves to a control plane, and how cloud native principles show up in its files |
| [Linux foundations](linux-foundations.md) | Processes, namespaces, cgroups v2, file descriptors and signals — hands-on, with the commands to inspect each |
| [Cloud native primer](cloud-native-primer.md) | How the engine uses namespaces, cgroups, capabilities, OCI, networking, CRI, KVM and reconciliation — with files and symbols |
| [Rust primer for this codebase](rust-primer.md) | The Rust this codebase actually uses |
| [Preparing your environment](environment.md) | What the kernel and host need, the pinned toolchain, and the host traps that look like engine bugs |
| [Clone, build and test](build-and-test.md) | Building, installing, running tests, every CI gate as a local command, E2E and chaos with isolation |
| [Project structure](project-structure.md) | What each top-level file and directory is, who changes it, and what is generated |
| [Architecture](architecture.md) | Layers, the crate graph, processes at runtime, control and data paths, state on disk |
| [The crates](crates.md) | One block per crate: responsibility, main types, where to start reading |
| [System Design Interview](system-design-interview.md) | The engine designed as an interview answer, then compared with what was built |
| [Delonixfile and VMfile](delonixfile-and-vmfile.md) | The build file grammars and how they differ from a Dockerfile |
| [Building microVMs](microvm-setup.md) | KVM, Cloud Hypervisor and firmware, libvirt, VM images |
| [Troubleshooting](troubleshooting.md) | A symptom index: gate failure messages, host traps and their fixes, in one place |
| [How names reach `/etc/hosts`](service-names-and-hosts.md) | The one delimited block, the two ways a name gets into it (`hosts: [host]` and `delonix hosts sync`), what it refuses, and how to test it without root |
| [Coding conventions](coding-conventions.md) | How code in this repository is written, and the checklist reviewers apply |
| [Adding a Kind](adding-a-kind.md) | The table, the schema and the reconciler wiring a new declarative Kind needs, worked through `Service` |
| [Contribution workflow](contributing-workflow.md) | Worktrees, versions, language rule, architecture rules, ADRs, commits and PRs |
| [Releases and stability](releases-and-stability.md) | The version gate, what a pushed tag does, and what the CLI and manifest schema promise not to break |
| [Publishing the documentation](publishing-docs.md) | How the site and this handbook are generated, gated and published |
| [Cloud native standards](cloud-native-standards.md) | Each standard, what it requires, how Delonix implements it, and its conformance status |
| [Environment variables](environment-variables.md) | Every `DELONIX_*` variable the code reads: who reads it, what it changes, its default, and which ones lower a boundary |
| [Glossary](glossary.md) | The engine and cloud-native terms you meet here, with their Delonix meaning and where to read more |

Other references you will be pointed to: [`ARCHITECTURE.md`](../../ARCHITECTURE.md) (C4 diagrams),
[`docs/adr/`](../adr/README.md) (architecture decisions), [`SECURITY.md`](../../SECURITY.md)
(private vulnerability reports) and [`CONTRIBUTING.md`](../../CONTRIBUTING.md) (the short front door).

---

**Next:** [Start here](start-here.md) — check your setup in thirty minutes and walk through a first contribution end to end.
