# 12. Coding conventions

This page tells you how to write code that passes review in this repository, so you don't have
to guess the rules or make up your own. Every rule below carries a tag and a source:

- **Enforced (gate)**: a CI job fails if you break it. The gate is named, so you can run it
  locally (see [02 — Build and test](02-build-and-test.md#the-gates-ci-runs)).
- **Decided (ADR/AGENTS.md)**: an **Accepted** Architecture Decision Record in `docs/adr/` or a
  section of `AGENTS.md` settles it. No gate checks it yet, so review does.
- **Proposed (ADR, not yet decided)**: the only written source is an ADR whose status is still
  **Proposed** (see the status column of [`docs/adr/README.md`](../adr/README.md)). It is the
  direction the code is moving in, and review applies it, but it can still change; when the ADR is
  accepted or rejected, the tag changes with it.
- **Convention (observed)**: the code does it consistently and nothing wrote it down. The
  examples are real `path:symbol` references. Copy them.

If a question is not answered here, and the code around your change doesn't answer it either,
the honest answer is **"not decided — follow the surrounding code"**. The list of known open
questions is in [Undecided](#undecided). Don't turn a personal preference into a rule in a PR.

Related pages: the layers and the reasons behind them in [05 — Architecture](05-architecture.md),
what each crate holds in [06 — Crates](06-crates.md), the Rust idioms in
[03 — Rust primer](03-rust-primer.md), and the workflow (worktree, version, ADRs, PRs) in
[10 — Contribution workflow](10-contributing-workflow.md).

---

## 1. The tooling that enforces style

| Tool | What it checks | Tag and source |
|---|---|---|
| **rustfmt** | Formatting. **No config file**: there is no `rustfmt.toml` or `.rustfmt.toml` at the root, so the defaults apply. | **Enforced (gate)**: CI job `fmt` runs `cargo fmt --all --check` (`.github/workflows/ci.yml`). `CONTRIBUTING.md` § Style: "`cargo fmt` defaults, no custom config". |
| **clippy** | Every clippy lint **and every rustc warning** fails the build, tests included (`--all-targets`). No `clippy.toml`, so the lint defaults apply. | **Enforced (gate)**: CI job `clippy` runs `cargo clippy --workspace --all-targets --locked -- -D warnings`. |
| **`[workspace.lints]`** | The root `Cargo.toml` declares exactly one workspace lint: `[workspace.lints.clippy] undocumented_unsafe_blocks = "deny"`. Every member manifest has `[lints] workspace = true`. | **Enforced (gate)**: clippy. It was introduced by phase P0 of ADR-0040 ("`[workspace.lints]` (`undocumented_unsafe_blocks = deny`)"), an ADR that is still **Proposed**; the gate holds regardless. |
| **cargo-deny** | RUSTSEC advisories and yanked crates, permissive licences only (no GPL/AGPL), crates.io as the only registry, no git sources. The `[bans]` section of `deny.toml` (duplicate versions and wildcards, set to warn) is **not** evaluated in CI. | **Enforced (gate)**: CI job `deny` runs `check advisories licenses sources` with `deny.toml`. **Convention (observed)**: every ignored advisory in `deny.toml` carries a comment with its reason — no gate checks that the comment exists. |
| **`scripts/lang_ratchet.py`** | Portuguese in identifiers, comments and user-facing strings (LANG-01, see [§2](#2-language-of-the-code)). | **Enforced (gate)**: CI job `lang`, baseline in `scripts/lang_baseline.json`. |
| **`scripts/arch_fitness.py`** | Layer direction, crate directory = layer, versions only in the root, consumer names, and the debt ratchets listed in [05](05-architecture.md#layers-and-the-allowed-direction) (see [§4](#4-structure-where-code-goes) and [§5](#5-internal-api-and-boundaries)). | **Enforced (gate)**: CI job `arch`, baseline in `scripts/arch_baseline.json`. |
| **`scripts/contract_gate.py`** | The node contract under `proto/delonix/node/v1` (see [§5.4](#54-the-node-contract)). | **Enforced (gate)**: CI job `contract`. |

Keep one mechanic in mind for **ratchets** (`lang_ratchet.py` and the `arch_fitness.py`
numbers). A ratchet fails when its number **rises**, and it **also** fails when the number falls
without the baseline being lowered in the same commit (`arch_fitness.py` prints "debt was paid;
lower the baseline in the same commit (--update)"). If you pay debt, run `--update` and commit the
new baseline together with the fix. A `<=` check would let the debt read as green forever (module
docstrings of both scripts).

A consequence you will meet: **Enforced (gate)**. Because `-D warnings` covers rustc's own lints,
rustc's naming lints (`non_snake_case`, `non_camel_case_types`, `non_upper_case_globals`) are
enforced too. `snake_case` functions and variables, `UpperCamelCase` types and `SCREAMING_SNAKE_CASE`
constants are not a style choice here.

`#[allow(clippy::…)]` exists in the tree, for example `#[allow(clippy::too_many_arguments)]` on
`crates/contexts/delonix-compute/src/run.rs` and `crates/contexts/delonix-compute/src/pod.rs`.
**Not decided**: no rule says when an `allow` is acceptable. If you add one, put a `// why`
comment next to it, as you would for any other exception (see [§10](#10-comments-and-documentation)).

---

## 2. Language of the code

- **Identifiers, comments and messages are written in English.** **Enforced (gate)**:
  `scripts/lang_ratchet.py`. **Decided**: AGENTS.md § "Língua do código: inglês (LANG-01)". The
  ratchet scans every `.rs`, `.py`, `.ts`, `.go` and `.yml`/`.yaml` file outside `target/`,
  `third_party/` and similar. It counts three things:
  - **identifiers**: any name declared by `fn`, `struct`, `enum`, `trait`, `const`, `static`,
    `type`, `mod`, `union` or `let` whose `snake_case`/`camelCase` segments contain a word from
    `scripts/lang_pt_lexicon.txt`;
  - **comments**: `//`, `///` and `//!` lines containing a lexicon word;
  - **user text**: literals of 8 or more characters inside `format!`, `println!`, `eprintln!`,
    `panic!`, `anyhow!`, `bail!`, `expect` or `unimplemented!`.
- **Portuguese reaches the operator only through the catalogue.** **Decided**: AGENTS.md §
  "i18n (fonte EN + catálogo pt.po embutido)" and `CONTRIBUTING.md` § "If you add or change a CLI
  command". The English text goes in the source. The Portuguese goes in
  `bins/delonix-runtime-bin/data/pt.po`, which `bins/delonix-runtime-bin/src/cmd/po.rs` embeds with
  `include_str!`.
  - `po::t("…")` for a fixed string.
  - `po::tf("… {name} …", &[("name", value)])` for interpolated text. Use **named** placeholders:
    a translation may reorder them, and `format!` needs a compile-time literal, so the template is
    translated first and the values are substituted afterwards (`po.rs`, doc comment of `tf`).

    ```rust
    // bins/delonix-runtime-bin/src/cmd/config.rs — refuse_unknown_key
    Err(Error::Invalid(super::po::tf(
        "'{key}' is not a config key — known: {known}",
        &[("key", key), ("known", &KNOWN_KEYS.join(", "))],
    )))
    ```
  - `--help` text is also English in the source, translated at runtime by `po::translate_help`.
    **Enforced (gate)**: `help_i18n_tests` in `bins/delonix-runtime-bin/src/main.rs`.
    `todo_o_help_de_comando_tem_traducao_pt` is strict for command help, and
    `o_help_dos_argumentos_so_pode_encolher` is a ratchet on `ARG_HELP_PENDING` for flag help. A new
    command or flag needs a `pt.po` entry.
  - A missing entry falls back to English, so the UI never goes blank (`po::t`). A Portuguese
    string written directly in the code is a bug, not a shortcut.
  - Don't reuse a `msgid` whose Portuguese depends on the grammatical gender of the subject.
    "created" can be *criada* for a network and *criado* for a volume, so use separate keys.
    **Decided**: AGENTS.md § "v0.32.2 — 380+ strings PT hardcoded".
- **Lexicon pitfalls.** **Decided**: AGENTS.md § LANG-01.
  - **Don't name anything `num`.** It is in the lexicon on purpose, because it catches real
    Portuguese ("num apply falhado"). A `num` identifier therefore counts as Portuguese debt and
    fails the gate. Use `count` or `number`.
  - Adding a word to the lexicon **raises** the count and fails the gate. Lower the baseline in the
    same commit. Homographs (`data`, `base`, `no`, `nas`…) are excluded unless a measurement shows they catch
    real Portuguese; `num` is the recorded exception (AGENTS.md § LANG-01).

---

## 3. Naming

### 3.1 Crates and directories

- **The directory is the layer.** A crate lives in `crates/foundation/`, `crates/contexts/`,
  `crates/adapters/`, `crates/providers/` or `crates/interfaces/`, or it is a binary in `bins/`. The
  directory must match the crate's entry in the `LAYERS` table. **Enforced (gate)**:
  `scripts/arch_fitness.py` `misplaced()` / `LAYER_DIR`. A new crate goes into `LAYERS` **and** the
  right directory in the same commit.
- **Target naming convention for new or restructured crates.** **Proposed (ADR, not yet
  decided)**: ADR-0040 D2.1.

  | Role | Name |
  |---|---|
  | Shared pure foundation | `delonix-model` |
  | Bounded context | `delonix-<context>`, named after the published API group (D2.2): `delonix-compute`, `delonix-stack` |
  | Technology adapter | `delonix-<technology>`: `delonix-linux`, `delonix-sdn`, `delonix-oci` |
  | Pluggable provider | `delonix-provider-<technology>` |
  | Interface library | `delonix-<protocol>`: `delonix-cri`, `delonix-mcp` |

  A crate exists only if it is a bounded context, isolates a heavy or privileged dependency, or is
  a separately installed binary. **No `-core`, `-common`, `-utils` or `-types` suffixes**
  (ADR-0040 D2.1 records how a `-core` crate became "the sink of everything"). Some crates still
  carry older names: `delonix-runtime-core`, `delonix-proxmox`, `delonix-truenas`,
  `delonix-security-runtime`. ADR-0040 renames each one in the phase that restructures it, "never
  twice". **Don't rename a crate outside its phase.**
- **Every crate's path is written once**, in `[workspace.dependencies]` of the root `Cargo.toml`.
  Members depend on each other with `{ workspace = true }`. **Decided**: AGENTS.md § "A direcção
  das dependências é um portão (ADR-0040, fase P0)", and the comment at the top of
  `[workspace.dependencies]`.

### 3.2 Modules and files

- **CLI command groups**: one module per group in `bins/delonix-runtime-bin/src/cmd/<group>.rs`,
  with a `pub enum <Group>Cmd` (clap subcommands), a `pub fn run(action: <Group>Cmd) -> Result<()>`
  dispatcher, and one `cmd_<verb>` function per subcommand. **Convention (observed)**:
  `cmd/volume.rs:VolumeCmd` + `run` + `cmd_create`/`cmd_ls`/`cmd_describe`;
  `cmd/secret.rs:SecretCmd` + `run`; `cmd/container.rs:cmd_run`/`cmd_start`/`cmd_stop`.
  AGENTS.md § "CLI (`delonix`)" states the one-module-per-group part.
- **Adapters for a compute port** go in a file named after the port's concern, not after the
  technology: `crates/adapters/delonix-linux/src/workload.rs`, `.../run_host.rs`,
  `crates/adapters/delonix-sdn/src/run_network.rs`, `.../vm_network.rs`,
  `crates/adapters/delonix-oci/src/run_images.rs`. **Convention (observed)**.

### 3.3 Types, traits, functions, constants

- **Ports are named after the capability, not the technology.** **Proposed (ADR, not yet
  decided)**: ADR-0040 D3 lists the ports: `WorkloadRuntime`, `SandboxProvider`, `VmProvider`, `NetworkProvider`, `StorageProvider`,
  `ImageRegistry`, `ImageStore`, … The ones that exist today are in
  `crates/contexts/delonix-compute/src/ports.rs` (`ImageStore`, `StorageProvider`, `DeviceResolver`,
  `RunHost`, `VmNetwork`, `NetworkProvider`) and `.../launch.rs` (`WorkloadRuntime`). The older
  `VmBackend` in `crates/adapters/delonix-vm/src/lib.rs` is due to become `VmProvider` in P4.
- **Host implementations of a port are called `Host<Thing>`.** **Convention (observed)**:
  `delonix-linux/src/workload.rs:HostWorkload` (implements `WorkloadRuntime`),
  `delonix-sdn/src/run_network.rs:HostNetwork` (implements `NetworkProvider`),
  `delonix-oci/src/run_images.rs:HostImages`, `delonix-volume/src/lib.rs:HostVolumes`,
  `delonix-linux/src/cdi.rs:HostDevices`.
- **Pure decision functions** are verbs or questions: `resolve_*`, `parse_*`, `valid_*`, `is_*`,
  `*_plan`. **Convention (observed)**: `cmd/vm.rs:resolve_vm_defaults`,
  `delonix-oci/src/registry.rs:parse_content_range`, `cmd/stack.rs:is_pending`,
  `delonix-net-rules/src/lib.rs:bridge_name`.
- **Constants** are `SCREAMING_SNAKE_CASE` (**Enforced (gate)**, rustc lint under clippy
  `-D warnings`). **Kind names are constants too, never repeated string literals**
  (**Decided**: AGENTS.md § "Os Kinds ganham grupos e nomes definitivos"; the constants are in
  `crates/contexts/delonix-stack/src/kinds.rs`: `pub const VM: &str = "VirtualMachine";`).

### 3.4 Test names

- **The ratchet counts test names.** `lang_ratchet.py` matches every `fn` declaration and does not
  skip `#[cfg(test)]`, so a Portuguese test name raises `identifiers` and fails the gate.
  **Enforced (gate)**.
- Many existing tests have Portuguese sentence names, for example
  `delonix-model/src/exitcode.rs:nao_existe_e_rebentou_deixam_de_ser_o_mesmo_numero`. That is
  counted debt, not a style to copy. New tests are **English sentences that state the behaviour
  being proven**, like the newer tests in the same file: `a_missing_capability_is_not_a_wrong_argument`
  and `the_text_class_and_the_number_cannot_diverge`. **Decided**: LANG-01 (AGENTS.md); the
  "sentence" shape is a **Convention (observed)**.
- If you translate an existing test name, the count goes down, so run
  `python3 scripts/lang_ratchet.py --update` in the same commit.

### 3.5 CLI commands and flags

- **Grouped commands, `delonix <group> <verb>`.** No flat top-level shortcuts. **Decided**:
  AGENTS.md § "Reorganização da raiz da CLI (v0.30.0)"; `docs/cli-stability.md` (the top-level
  shortcuts were removed in v1.0.0).
- **Verbs follow Docker/Podman/kubectl** when such a verb exists. **Decided**: AGENTS.md § the
  "Reestruturação da CLI (semântica Docker/Podman/kubectl)" sprints; `docs/cli-stability.md` §
  "Estável".
  - Listing verbs use `ls` (`network ls`, `volume ls`, `image ls`…). `image list` was reverted to
    `ls` in v2.0.0 (`docs/cli-stability.md`).
  - `create` only creates, and refuses an existing name with exit 5 unless `--force`. Upsert is a
    separate verb (`secret set`). `apply` is idempotent "ensure present". **Decided**: AGENTS.md §
    "Sprint 1: `secret create` vs `secret set`".
  - `describe` is for humans (kubectl style), `inspect` is JSON for scripts. **Decided**: AGENTS.md
    § "Output: `ls` estilo docker, `describe` estilo kubectl".
  - Flag order and names copy Docker where Docker has the concept:
    `network connect <NETWORK> <CONTAINER>`, `-p [hostIp:]hostPort:containerPort`,
    `volume create --driver … --opt k=v`. **Decided**: AGENTS.md § Sprints 5 and 6.
- **Breaking changes are clean cuts, without aliases.** The old spelling must fail with
  `unrecognized subcommand`, never silently do something else. Before you cut, grep for internal
  callers across the **whole** workspace. **Decided**: `docs/cli-stability.md` § "Como uma quebra é
  feita". The groups listed as stable in that file may only break in a major release.
- **A command reachable from several paths must be wired on all of them** (for example
  `vm pull` / `image vm pull` / `image --vm pull`). **Decided**: `CONTRIBUTING.md`; see
  [10](10-contributing-workflow.md#adding-or-changing-a-cli-command).
- **Leaf changes update the CLI baseline** (`scripts/cli-tree.sh --update`) in the same commit.
  **Enforced (gate)**: see [10](10-contributing-workflow.md#adding-or-changing-a-cli-command).

### 3.6 Kinds, API groups and manifest fields

- **Kinds are `UpperCamelCase` nouns** in one of the published groups
  `core`, `compute`, `networking`, `gateway`, `storage`, `artifact`, `infrastructure`
  (`<group>.delonix.io/v1alpha1`). **Decided**: AGENTS.md § "Identidade e fronteira do
  motor" and § "Os Kinds ganham grupos" (ADR-0020, which introduced the groups, is still
  **Proposed**). Each Kind is **one row** in
  `crates/contexts/delonix-stack/src/kinds.rs` (`KindFacts`: `kind`, `plural`, `short`,
  `api_version`, `domain`, `form`, `in_stack`, `converges`, …). `delonix api-resources` prints that
  table. Adding a Kind also touches tables that nothing derives from `kinds.rs` (`hot_fields`,
  `NAMESPACE_SOURCES`, `TYPED_KINDS`, the generated schema). The tests fail until each one is done.
  **Enforced (gate)**: AGENTS.md § "`kind: Service`" lists which test caught each table.
- **A renamed Kind keeps its old name as a silent, case-insensitive alias.** A **merge** warns,
  because its meaning changed. **Decided**: AGENTS.md § "Os Kinds ganham grupos e nomes
  definitivos" ("Alias silencioso, não depreciação"); implemented in
  `cmd/manifest.rs:KIND_ALIASES`. ADR-0020 is still **Proposed**.
- **Manifest fields are `camelCase`.** If a field had a `snake_case` spelling before, that spelling
  stays accepted as a `serde` `alias`. **Convention (observed)**, per field rather than
  `rename_all`:

  ```rust
  // bins/delonix-runtime-bin/src/cmd/vm.rs — VmSpec
  /// Canonical `cpuAffinity`; `cpu_affinity` stays accepted (back-compat).
  #[serde(rename = "cpuAffinity", alias = "cpu_affinity")]
  cpu_affinity: Option<String>,
  ```

  Other examples: `delonix-compute/src/pod.rs:PodSpec.restart_policy` (`rename = "restartPolicy"`),
  `cmd/service.rs:ServiceSelector.match_labels` (`rename = "matchLabels"`). The published schema
  (`docs/schema/v1/delonix.json`) is **generated** from these structs and is tested to match them
  (ADR-0007). The manifest schema is declared **stable** (`docs/cli-stability.md` § "O schema dos
  manifestos").
- **Internal records** (the JSON under the state root) keep Rust's `snake_case` field names. See
  `crates/foundation/delonix-runtime-core/src/lib.rs` (`net_mode`, `namespace`). **Convention
  (observed)**.

### 3.7 Environment variables

- **Prefix `DELONIX_`**, upper case: `DELONIX_ROOT`, `DELONIX_NET_RUNTIME_DIR`, `DELONIX_L18N`,
  `DELONIX_LOG_FORMAT`, `DELONIX_CRI_CAP_CEILING`. **Convention (observed)** across `crates/` and
  `bins/`.
- **For telemetry, read the standard `OTEL_*` variables**, not a new `DELONIX_*` alias.
  **Proposed (ADR, not yet decided)**: ADR-0040 D6. This is the target, not today's code:
  `crates/adapters/delonix-telemetry/src/telemetry.rs` reads `DELONIX_OTLP_ENDPOINT` for the OTLP
  exporter, and from the standard set only `OTEL_SERVICE_NAME`. Don't add a new `DELONIX_*`
  telemetry variable, and don't remove `DELONIX_OTLP_ENDPOINT` outside the phase that migrates it.
- **An escape hatch that weakens a safety default is loud and explicit.** It is off unless set to
  `1`, and it warns: `DELONIX_ENABLE_IPV6=1`, `DELONIX_ALLOW_LINK_LOCAL=1`. **Decided**: AGENTS.md §
  "Bloco 0 do plano 33 (v0.37.1)".
- A flag beats the environment variable, which beats the default (`serve cri --cap-ceiling` vs
  `DELONIX_CRI_CAP_CEILING`). **Decided**: AGENTS.md § "Tecto de capabilities no CRI".

### 3.8 Exit codes and `DX_*` codes

- **Exit codes are derived from the error type in one place**: the exhaustive
  `crates/foundation/delonix-model/src/exitcode.rs:for_error`. The classes are 1 generic,
  2 usage, 3 `NOT_RUNNING`, 4 `NOT_FOUND`, 5 `CONFLICT`, 69 `UNAVAILABLE`, 74 `IO`,
  77 `NO_PERMISSION`, 124 `TIMEOUT`. Each error also has a stable text identity, `Error::code()`,
  which returns a `DX_*` string. **Enforced (gate)**: the match has no `_ =>` arm, so a new variant
  stops the build until someone classifies it. The test
  `the_text_class_and_the_number_cannot_diverge` keeps the two in step. **Decided**: AGENTS.md §
  "Códigos de saída com classe (v0.49.0)"; `docs/cli-stability.md` § "Códigos de saída".
- **You don't pick a number, you return the right variant.** "It doesn't exist" is
  `Error::NotFound`, "it already exists" is `Error::Conflict`, "this host lacks a tool" is
  `Error::Unavailable`. A new number needs a real producer. **Decided**: module docs of
  `exitcode.rs` ("every extra number is a promise").

---

## 4. Structure: where code goes

### 4.1 The layers and the direction

The allowed direction is written in one place, `ALLOWED` in `scripts/arch_fitness.py`.
**Enforced (gate)**:

| Layer | May depend on |
|---|---|
| foundation | foundation |
| context | foundation, context |
| adapter / provider | foundation, context |
| interface | foundation, context, adapter, provider |
| bin | everything |

Dev- and build-dependencies don't count. A declared exception must name the ADR-0040 phase that
removes it, and an exception that no longer applies also fails (`EXCEPTIONS`). The generated layer
table and current exceptions are in [05](05-architecture.md#layers-and-the-allowed-direction).

More structural rules, each **Enforced (gate)** by `scripts/arch_fitness.py`:

- **Foundation and contexts stay pure of heavy dependencies.** `tokio`, `axum`, `hyper`, `tonic`,
  `reqwest`, `clap`, `ratatui`, `serde_yaml`, `rmcp`, OpenTelemetry and `prometheus-client` are
  refused there (`HEAVY`).
- **A binary composes exactly one interface.** Enforced in `rule_failures()` (introduced by
  ADR-0040 D2.4, still **Proposed**; also written in AGENTS.md § "A direcção das dependências é um
  portão").
- **Dependency versions live only in the root `[workspace.dependencies]`.** A member writes
  `{ workspace = true, features = [...] }` and nothing else. `default-features = false` stays in the
  root, because a member cannot turn off what the root turns on (`inline_versions()`).
- **Libraries don't print.** The `library_prints` ratchet counts `println!`/`eprintln!`/`print!`
  outside `bins/`. Emit `tracing` instead (for example `tracing::warn!` in
  `crates/adapters/delonix-sdn/src/lib.rs`) and let the interface present the output.
  **Decided**: AGENTS.md § "A direcção das dependências é um portão (ADR-0040, fase P0)" ("Uma
  biblioteca não escreve para o terminal; emite `tracing`").
- **Libraries don't re-run the engine's own binary.** The `self_exec_sites` ratchet counts
  `current_exe()`, `cli_bin()` and `delonix_bin()` outside `bins/`. Call a function or a use case
  instead. `Command::new("ip")`, `nft`, `qemu-img` and `ssh` are **not** counted, because running
  those tools is exactly what an adapter is for (comment above `SELF_EXEC`).
- **Don't write the process environment.** The `env_writes` ratchet counts
  `env::set_var`/`remove_var` everywhere, tests included. Tests run on parallel threads and a write
  races every reader. Pass values in instead (comment above `ENV_WRITES`).

### 4.2 "My change is X → it goes in Y"

This table uses the crates as they exist today. Check [06](06-crates.md) for each crate's contents
before you add to it. Where the source column cites ADR-0040 or ADR-0026, the tag is **Proposed
(ADR, not yet decided)**: both ADRs are still Proposed, even though the crates they describe
already exist.

| Your change | Crate (layer) | Source |
|---|---|---|
| A pure rule on CIDRs, bridge names or IPAM arithmetic that both sides must compute identically | `delonix-net-rules` (foundation) | 06; AGENTS.md § Arquitetura |
| A new error class, exit code or `DX_*` code; generated names | `delonix-model` (foundation) | ADR-0040 D1 |
| A persisted record type, a store, the secret store, `write_atomic*` helpers | `delonix-runtime-core` (foundation) — being split by ADR-0040 P3. Add here only what belongs to records and stores, never general helpers | ADR-0040 D2.1 "no `-core`" |
| Kind facts, the planner/diff, conditions, revisions | `delonix-stack` (context) | AGENTS.md § Arquitetura |
| The run specification, its pure validation, a port the run use case needs | `delonix-compute` (context): `run_opts.rs`, `preflight.rs`, `ports.rs` | ADR-0040 D2.2 |
| Security policy, admission, score, redaction | `delonix-security-runtime` (context) | ADR-0026 |
| Namespaces, cgroups, mounts, capabilities, seccomp, devices | `delonix-linux` (adapter) | ADR-0040 D2.3 |
| netns holder, nftables, slirp, DNS, DHCP, overlay, WireGuard, CNI | `delonix-sdn` (adapter) | ADR-0040 D2.3 |
| Registry client, CAS, layers, overlay, image build | `delonix-oci` (adapter) | ADR-0040 D2.3 |
| SBOM / CVE | `delonix-scanner` (adapter) | ADR-0040 D2.3 |
| Tracing, OpenTelemetry, Prometheus registry setup | `delonix-telemetry` (adapter) | ADR-0040 D2.3 |
| A local VM backend (Cloud Hypervisor, libvirt) | `delonix-vm` (adapter) | ADR-0008 |
| A remote or pluggable provider (hypervisor API, NAS API) | a provider crate in `crates/providers/`. **Write an ADR first** | ADR-0008, ADR-0009; [10](10-contributing-workflow.md#when-to-write-an-adr) |
| A CRI RPC | `delonix-cri` (interface) | AGENTS.md |
| The local management API, `/metrics` | `delonix-mgmt` (interface) | ADR-0010 |
| An MCP tool | `delonix-mcp` (interface) | ADR-0025 |
| A CLI command, its presentation and translations | `bins/delonix-runtime-bin/src/cmd/<group>.rs` + `data/pt.po` | AGENTS.md § CLI |
| Which adapter backs which port (composition) | the binary's composition root. No business logic there | ADR-0040 D1 "Binaries" |

### 4.3 Pure core, I/O at the edges

- **Decisions are pure functions over data you have already read.** They take no store, run no
  command and need no privilege, so a test can call them with plain values. **Proposed (ADR, not yet
  decided)**: ADR-0040 D1 (a context's `domain/` has "no I/O, no `tokio`, `libc`, `nix`,
  `std::fs`"). **Convention (observed)**: `delonix-stack/src/reconcile.rs` ("decides it WITHOUT touching the machine"),
  `cmd/vm.rs:resolve_vm_defaults`, `delonix-sdn/src/infra.rs:vmtap_line`,
  `delonix-oci/src/registry.rs:parse_content_range`.

  ```rust
  // bins/delonix-runtime-bin/src/cmd/stack.rs — is_pending
  fn is_pending(present: &str, kind: &str, status: &str) -> bool {
      match present {
          // Declarative: nothing to observe, so nothing to wait for.
          "-" => false,
          "yes" => !ready_status(kind, status),
          // "no" (absent) and "?" (unknown/unreadable) both keep waiting.
          _ => true,
      }
  }
  ```

- **If a pure function needs something from the outside, take it as a parameter.** For example,
  `resolve_image_ref` takes the image store instead of opening the real one, so the test can pass a
  temporary directory. **Decided**: AGENTS.md § "O manifesto de VM resolvia a imagem de outra
  maneira que a CLI".
- **One rule, one owner.** When two call sites need the same derivation, extract one function and
  call it from both. A second copy drifts. Examples: `delonix_net_rules::bridge_name`, re-exported by `delonix-sdn` (the bridge name
  had two formulas and printed a device that did not exist), `infra::dhcp_lease_ip`,
  `effective_entrypoints`. **Decided**: AGENTS.md § "`delonix network`", § "Isolamento de
  namespace", § "Reverse-proxy L7".

---

## 5. Internal API and boundaries

### 5.1 Ports and adapters

- **A new backend implements a port. It is never an `if provider == …` somewhere else.**
  **Decided**: AGENTS.md § "Identidade e fronteira do motor" ("Um provider novo entra como
  implementação de uma porta, nunca como um `if provider == …`"). ADR-0040 D3 rule 3 ("No string
  matching on provider names outside the composition root") restates it and is still **Proposed**.
  D3 plans a fitness test for this, but
  **it does not exist yet in `arch_fitness.py`**, so review enforces it for now.
- **Backend-specific knowledge lives on the backend.** For example,
  `VmBackend::ip_is_predicted()` answers whether a VM's IP was predicted, instead of the call site
  checking `backend.contains("cloud-hypervisor")`. **Decided**: ADR-0008, quoted in the doc comment
  in `crates/adapters/delonix-vm/src/lib.rs`.
- **An adapter does not depend on another adapter.** What it needs from another concern comes in
  as a hook or a port, wired by the composition root. **Enforced (gate)**: `ALLOWED` (adapter →
  foundation, context). **Convention (observed)**: the doc comment of
  `delonix-linux/src/workload.rs:HostWorkload` explains its `addresses`/`attach_slirp` hooks this
  way.

  ```rust
  // crates/contexts/delonix-compute/src/ports.rs — NetworkProvider (excerpt)
  pub trait NetworkProvider {
      /// Refuses a network that does not exist.
      fn check_network(&self, name: &str) -> Result<()>;
      /// Undoes an attach; best effort, used on the way out of a failure.
      fn detach(&self, id: &str, ip: &str);
      /// Publishes one `-p` specification on a container's address.
      fn publish(&self, ip: &str, spec: &str) -> Result<()>;
  }
  ```

- **A trait needs a real consumer when it lands.** Don't write scaffolding that waits for its first
  caller. Every method must have a caller. **Decided**: AGENTS.md § "`delonix workload`"
  (ADR-0002). A public function with no callers is also a known hazard: several of them turned out
  to hide a latent bug (`mount_live`, `set_net_rate`, `update_limits`, `publish_port_allow`), and
  some were deleted rather than wired (AGENTS.md § "Endurecimento do ingress/egress").

### 5.2 Errors per crate (ADR-0040 P3)

- **An adapter or provider defines its own `Error` and converts it into the shared class**
  (`delonix_model::Error`), which carries the `DX_*` code. **Proposed (ADR, not yet decided)**:
  ADR-0040 P3.
  **Enforced (gate)**: the `shared_error_imports` ratchet in `arch_fitness.py` (`SHARED_ERROR`,
  limited to `crates/adapters/` and `crates/providers/`) counts
  `use delonix_runtime_core::{…Error/Result…}` or `use delonix_model::{…}` imports that make the
  shared type the crate's own result type. The shared type may still be named inside a `From`
  impl. The reference implementation is `crates/adapters/delonix-scanner/src/error.rs`:

  ```rust
  impl From<Error> for Dx {
      fn from(e: Error) -> Self {
          match e {
              e @ (Error::EmptySbom | Error::OsvShape | /* … */ Error::NoModule) => Dx::Invalid(e.to_string()),
              Error::ModuleScan(io) => Dx::Runtime { context: "module scan", message: io.to_string() },
              Error::Engine(e) => e,
          }
      }
  }
  ```

  Three things in that file are the pattern: the conversion **decides** the class; `code()` asks the
  conversion instead of keeping a second table; and a test (`the_code_is_the_code_of_the_class_it_converts_into`)
  keeps the two in step. The converted message is also kept byte-for-byte identical to what the CLI
  printed before (`the_converted_message_is_the_one_printed_before`).
- **Ask the error its class; don't match on a variant of the shared error.** Outside the
  foundation, write `e.is_not_found()` or `e.class()`, or match on `e.root()` when you need the
  payload — never `Err(Error::NotFound(_))` or `matches!(…, Error::NotFound(_))`. A crate's own
  error travels inside the shared class with its code, so a variant match stops matching it
  without a word from the compiler. **Decided**: ADR-0043 D4 (Accepted).
  **Enforced (gate)**: the `raw_error_variant_matches` ratchet in `arch_fitness.py`
  (`RAW_VARIANT_MATCH`, skipping `crates/foundation/delonix-model/`), whose baseline is 0 — any
  new raw match fails CI. The methods live in `crates/foundation/delonix-model/src/codes.rs`.

### 5.3 Visibility

- **Default to private.** Inside the CLI crate, use `pub(crate)` for something another `cmd`
  module needs to call. **Convention (observed)**: `cmd/container.rs:cmd_run`, `cmd_start` and
  `cmd_stop` are `pub(crate)` so that `pod`, `compose` and `stack` can delegate to them;
  `cmd/firewall.rs:update_locked`; `cmd/manifest.rs:KIND_ALIASES`.
- **`pub` in a library crate is a promise to other crates.** Removing a public item is a breaking
  change for library users, even with zero callers in this workspace. rustc's `dead_code` lint
  doesn't see unused `pub` items, so when you delete one, count the orphaned public items by hand.
  **Decided**: AGENTS.md § "`delonix_sdn::Net` foi APAGADO — e é breaking para quem usa a biblioteca".

### 5.4 The node contract

`proto/delonix/node/v1` is the source of truth for both the gRPC and the HTTP/JSON encodings.
`docs/api/openapi.yaml` is generated from it. **Never edit the OpenAPI file by hand.**
**Enforced (gate)**: `scripts/contract_gate.py` runs `buf format`, `buf lint`, `buf breaking`
against the last tag that contains `proto/`, checks the HTTP mapping of every RPC, and checks that
the OpenAPI file equals the generated one. **Decided**: AGENTS.md § "O contrato de nó é um portão"
(which cites ADR-0040 P1; ADR-0040 D4 is still **Proposed**).

- **One request message per RPC**, named `<Rpc>Request`. **Enforced (gate)**: `buf lint`
  `RPC_REQUEST_STANDARD_NAME` (see `buf.yaml`). A shared request lets a field meant for one method
  show up in five.
- **Explicit identity in the request**: `name` and `namespace` fields, never a generic metadata
  message with fields the engine would ignore. **Decided**: AGENTS.md. **Convention (observed)**:
  `compute.proto:GetContainerRequest { string name = 1; string namespace = 2; }`.
- **Images are addressed by query, not in the path**, because in a path like `alpine:3.20` the `:`
  would be read as a custom verb. **Decided**: AGENTS.md. **Convention (observed)**:
  `infra.proto` `GetImage` → `get: "/v1/images:get"`, with `GetImageRequest { string reference = 1; }`.
- **Responses** return the resource, or an `Operation` for long-running mutations. `buf lint`'s
  response-naming rules are switched off on purpose (`buf.yaml` comment).
- **Every RPC has an HTTP mapping except the bidirectional streams** (`Exec`, `Console`), which must
  not have one. **Enforced (gate)**: `contract_gate.py` check 4.

### 5.5 The engine knows no consumer

The engine doesn't know who uses it. No name of a platform, control plane, console or agent, and
no notion of tenant, account, plan or billing, may appear in `crates/`, `bins/`, `proto/`, the root
`Cargo.toml` or the `Makefile`, **comments included**.

- **Consumer names.** **Enforced (gate)**: `scripts/arch_fitness.py` `consumer_mentions()` matches
  the `CONSUMER_NAMES` regular expression, a fixed list of names, in those paths. A name that is
  not on the list is not caught.
- **Tenant, account, plan and billing concepts.** **Decided**: AGENTS.md § "Identidade e fronteira
  do motor". No gate matches them; review checks it.

If a consumer needs something, write it as a generic
capability in the engine's own vocabulary (Kinds and resources), and add it only if it makes sense
for any client. The engine validates its own contract and never trusts a caller to refuse what it
doesn't support.

---

## 6. Error handling and messages

- **No silent failure.** If an option is accepted and then ignored, that is worse than a missing
  feature, because the user believes it took effect. Refuse it with a clear error, and name the
  flag. **Decided**: AGENTS.md § "Falhas silenciosas corrigidas (fail-closed)"; the v0.37.0 audit
  (§ "Auditoria sistemática dos 208 subcomandos") calls this class "relato desonesto" (dishonest
  reporting). The patterns that section lists are:
  - **Don't destroy anything before you know the object is yours to destroy, and delete the
    bookkeeping last.** If the record is removed first and the data removal then fails, the data
    is orphaned and a later `create` hands it to someone else.
  - **An unreadable measurement is *unknown*, never zero.** A `read_dir` that fails is not an empty
    directory. That is why `Usage { bytes, unreadable }` exists.
  - **Watch the patterns that turn failures into success**: `let _ =` on a result that matters
    (entropy, a socket read), `as u64` on an `f64` (it saturates), `capture()` read by its `Result`
    instead of its output. AGENTS.md § "A classe «X não é Y»" catalogues these.
- **Unknown or unmeasurable is not a guess.** When the engine can't read a value, it reports that
  it doesn't know, or it refuses. It doesn't pick the likelier answer. Example: `system prune --auto`
  refuses if disk usage can't be read, and the IPAM reaper fails closed when a store is unreadable
  (an empty list would read as "nothing is alive"). **Decided**: AGENTS.md § CLI (`system prune`),
  § "O IPAM vaza".
- **Message shape: the fact first, then what to do**, with the exact command when there is one.
  **Decided**: AGENTS.md § "`-p 80:80` respondia com o JSON cru do slirp" ("facto primeiro, depois
  os comandos prontos a copiar"). **Convention (observed)**:
  `delonix-model/src/error.rs:Error::VmNotFound` → `"no such VM: {0} (see \`delonix vm ls\`)"`;
  `cmd/config.rs:refuse_unknown_key` names the valid keys. Name the missing **tool and its
  package** instead of passing on a raw `ENOENT`, which reads like a missing file (AGENTS.md §
  "A bateria mede o `--help` de tudo", achado 1).
- **A partial measurement is not a success.** `--wait` must observe what it claims, and `✓ … is up`
  is only printed after checking. **Decided**: AGENTS.md § "O `--wait` de uma VM CH".
- **Never parse an error message to decide what to do.** Messages are translated
  (`--l18n=pt`/`DELONIX_L18N`), so a `grep 'no such'` classifies on one machine and silently stops
  classifying on another. Use the exit class or the `DX_*` code. **Decided**: module docs of
  `delonix-model/src/exitcode.rs`.
- **Return the variant that matches the class** (`NotFound`, `Conflict`, `NotRunning`,
  `Unavailable`, `Timeout`), not `Invalid` for everything. **Decided**: AGENTS.md § "Códigos de
  saída com classe": `util::find` returning `Invalid` for "not found" made the most-used resource
  unclassifiable.

---

## 7. `unsafe`, syscalls and processes

- **Every `unsafe` block has a `// SAFETY:` comment directly above it.** **Enforced (gate)**:
  `undocumented_unsafe_blocks = "deny"` in `[workspace.lints.clippy]`.

  ```rust
  // crates/adapters/delonix-linux/src/lib.rs — apply_filter_logged
  // SAFETY: `fprog` points to a valid BPF program; NO_NEW_PRIVS is already set.
  let rc = unsafe {
      libc::syscall(libc::SYS_seccomp, SET_MODE_FILTER, FLAG_LOG, &fprog as *const _)
  };
  ```

  The comment has to state the invariant that makes the call sound. "same" is acceptable only
  directly after an identical, justified call (as in `delonix-linux/src/lib.rs` right after the
  first `_exit(126)`).
- **No raw `clone()`/`fork()` in a multi-threaded process** (the tokio servers, the Docker API
  shim). `clone` doesn't run `pthread_atfork` handlers, so the child can deadlock on the malloc lock.
  Re-execute a typed spec instead, handed over through a `0600`/`O_EXCL` file rather than argv.
  **Decided**: AGENTS.md § "Auditoria de segurança #3", item 5. Background:
  [03](03-rust-primer.md#why-forkclone-in-a-multi-threaded-process-is-dangerous).
- **A `pre_exec` hook must not block on something the parent does after `spawn` returns.**
  `Command::spawn` only returns after `exec`, so the two processes wait for each other forever.
  Use a raw `fork` for handshakes. **Decided**: AGENTS.md § "A classe «X não é Y»" (the
  `reexec_mapped_hold` entry).
- **Temporary files: use `delonix_runtime_core::write_private_temp`.** It opens with a unique name,
  `O_EXCL` and mode `0600`, so it never follows a planted symlink. Don't use a fixed or pid-derived
  name in `/tmp`. **Decided**: AGENTS.md § "Auditoria de segurança #3", passagem 2; doc comment in
  `store.rs`. **Convention (observed)**: `delonix-sdn/src/bpf.rs`,
  `delonix-linux/src/run_host.rs`.
- **Files that must be private or atomic: use `write_atomic_mode(path, bytes, Some(0o600))`.** It
  sets the mode at creation and publishes with an atomic rename. Never write the file and then
  `chmod` it, because another user can open it in between. **Decided**: doc comment of
  `store.rs:write_atomic_mode`; AGENTS.md (kubeconfig TOCTOU).
- **Before signalling a pid read from a file, check that it is still the same process.** Use
  `delonix_runtime_core::safe_to_signal(pid, starttime)`, which compares the start time so a
  recycled pid isn't killed. **Decided**: AGENTS.md § "A classe «X não é Y»" (the pid entries).
- **A process's argv doesn't prove it is ours.** Other state roots of the same user, and other
  tools, run with the same argv. Check a token only we choose: a path derived from our root, or an
  environment variable we pinned at spawn. **Decided**: AGENTS.md § "A classe «X não é Y»" (the
  "o argv de um processo" entry).
- **Pass `--` before positional arguments that come from input** in the argv of external tools
  (`ssh`, `scp`, `virsh`, `mount`, `qemu-img`), and validate any value that ends up in a remote
  shell against a character whitelist. `shell_quote` does not sanitise content. **Decided**:
  AGENTS.md § "Auditoria de segurança (skill `delonix-runtime-sec`)" and § "#2".
- **Paths built from names in user or manifest input are confined.** Use a `valid_*` name check at
  the engine boundary (`delonix_vm::valid_vm_name`), and a safe join that refuses `..`/absolute
  components and symlinks (`safe_join`, `safe_bind_target`). **Decided**: same AGENTS.md sections.

---

## 8. State and concurrency

- **Read–modify–write goes through `update`, never `load` → mutate → `save`.**
  `Store::update` and `JsonStore::update` (`crates/foundation/delonix-runtime-core/src/store.rs`)
  take a `flock`, **re-read under the lock**, apply your closure and write atomically. A closure
  that returns `false` aborts the write. The CLI, the CRI server and background refreshes all touch
  the same records concurrently, and without the lock one write is silently lost. **Decided**: doc
  comments of both functions; AGENTS.md § "Revisão ampla de código/arquitectura (2026-07-27)",
  bugs 5 and the `JsonStore` item. **Convention (observed)**: a mutation that can itself fail is
  wrapped like this:

  ```rust
  // bins/delonix-runtime-bin/src/cmd/firewall.rs — update_locked
  let c = store.update(id_or_name, |c| match f(c) {
      Ok(commit) => commit,
      Err(e) => {
          err = Some(e);
          false
      }
  })?;
  ```

- **Persist each step as soon as the dataplane confirms it.** If a multi-step change fails
  halfway, the record must still match what the kernel actually has. **Decided**: AGENTS.md §
  "Reconfiguração a quente" ("Persistência").
- **New fields on persisted records take `#[serde(default)]`** (or `default = "fn"`), so records
  written by older versions still load. The default must describe what old records actually were,
  not a guess. **Decided**: AGENTS.md (for example `Vm.namespace`, `VmImage.cloud_init`).
  **Convention (observed)**: `delonix-runtime-core/src/lib.rs`, the doc comment on `Vm.namespace`
  ("the default is a statement of fact and not a guess").
- **Anything needed to rebuild a resource must be persisted, not only used at creation.** When you
  touch a `start`/`restart` path, compare field by field what creation uses with what the record
  stores. **Decided**: AGENTS.md § "BUG GRAVE corrigido… `-v` nunca era persistido" (listed there as
  the third bug of the same family).
- **Lock files are never deleted.** Deleting one opens a window where two processes lock different
  inodes. **Decided**: doc comment of `store.rs:lock_path`.

---

## 9. Tests

- **Where they go.** Unit tests go in `#[cfg(test)] mod tests` at the bottom of the file. Integration
  tests go in `crates/<layer>/<crate>/tests/` and use only the public API. Live provider tests are
  opt-in. **Convention (observed)**; details in [03 § 3.9](03-rust-primer.md#39-tests).
- **Pure first.** Put the decision in a pure function and test it as data. Anything that needs
  real namespaces, cgroups or a network holder is validated live or with `scripts/e2e.sh`.
  **Decided**: `CONTRIBUTING.md` ("Write a unit test for any new pure function"); AGENTS.md §
  "IaC nativo" (`reconcile.rs` is pure so it can be tested as data).
- **Names**: English sentences that state the behaviour (see [§3.4](#34-test-names)).
- **Tests never touch the host's real state.** Give stores a temporary root. Don't call code that
  resolves the real state root. Don't `set_var` (the `env_writes` ratchet, [§4.1](#41-the-layers-and-the-direction)).
  **Decided**: AGENTS.md § "IaC nativo", the `ShareVolume` fusion note ("Nota de método: um teste que
  chamasse `apply_share` … escreveria no estado REAL da máquina"). **Convention (observed)**:
  `delonix-runtime-core/src/store.rs` tests use a `tmp_dir(tag)` helper. For manual and E2E runs,
  isolate **both** `DELONIX_ROOT` and `DELONIX_NET_RUNTIME_DIR`. Isolating only one is worse than
  none (AGENTS.md § "Meia-isolação é pior que nenhuma"; [02](02-build-and-test.md#isolating-the-engines-state)).
- **A regression test must fail with the fix reverted.** Revert the fix, watch the test fail, then
  restore the fix. A test that passes either way proves nothing, and AGENTS.md records several
  (an exit-code check that `1` couldn't distinguish; a chaos scenario that stayed green with a
  reversion). **Decided**: AGENTS.md, "verificado pela regra do repo" throughout, for example §
  "IaC nativo" (`stack_converge`) and § "A bateria mede o `--help`".
- **Test the path production uses.** If production passes relative paths, the test uses relative
  paths. A test can encode the bug. **Decided**: AGENTS.md § "Auditoria sistemática dos 208
  subcomandos" (`default_project_name`).
- **Concurrency bugs get a real race.** Use threads plus an explicit sleep inside the critical
  window. **Convention (observed)**: `store.rs:jsonstore_update_concorrente_nao_perde_escritas`.
- **Prefer properties over sampled timing.** When a race can only be sampled, generate load and
  repeat. **Decided**: AGENTS.md § "Um `exec` logo a seguir ao `run -d` corria no HOST".
- **Generated regions carry no volatile counts** (lines, tests, commits). **Decided**:
  `scripts/dev_docs.py` docstring ("Deliberately NOT generated: line counts, test counts, commit
  counts"). A **hand-written measurement** cites the measured value together with the date it was
  measured, never a running total. **Decided**: AGENTS.md § "A bateria mede o `--help` de tudo e
  EXECUTA um quarto" ("Cita-se a fracção medida e a data, nunca o total"). Whether counts may appear
  in prose or code comments at all is **not decided** — follow the surrounding text.

---

## 10. Comments and documentation

- **Comments explain *why*, not *what*.** Write one for a hidden constraint, a workaround for a
  specific bug, or an invariant the code doesn't make obvious. Comments that restate the code are
  removed in review. **Decided**: `CONTRIBUTING.md` § Style.
- **When a decision was measured, say what was measured.** "Measured: …" beats "should". The
  module docs in `exitcode.rs` and `reconcile.rs` are the model. **Convention (observed)**.
- **Doc comments (`///`, `//!`) on public items and module headers** say what the item promises and
  why it exists. **Convention (observed)**: every port in `delonix-compute/src/ports.rs`,
  `store.rs:write_private_temp`, `exitcode.rs`. **Not decided**: no `missing_docs` lint is enabled.
- **Comments are English and name no consumer.** The language ratchet and the consumer gate both
  scan comments. **Enforced (gate)**.
- **Don't add abstractions, config flags or error handling for cases that can't happen.**
  **Decided**: `CONTRIBUTING.md` § Style.
- **When you move a boundary, update its records in the same change**:
  - an ADR **before** the code, for a new provider, port, daemon, external dependency in an engine
    crate, privilege boundary, or change to the contract or layers. Accepted ADRs are never
    rewritten; a new one supersedes them (**Decided**: `docs/adr/README.md`;
    [10](10-contributing-workflow.md#when-to-write-an-adr));
  - the `AGENTS.md` section that describes the area. A stale section there misleads the next
    person, and **conflicts in `AGENTS.md` are resolved by keeping both sides** (**Decided**:
    AGENTS.md § "Método: um worktree por sessão");
  - the generated artefacts: `docs/schema/v1/delonix.json`, `docs/api/openapi.yaml`, the CLI
    baseline, `docs/gen.py`, and the `docs/dev/` generated regions via
    `python3 scripts/dev_docs.py` (**Enforced (gate)**; see
    [11](11-publishing-docs.md)).

---

## Undecided

Nothing in the repository settles these. Follow the surrounding code and mention the choice in
your PR:

- **When `#[allow(clippy::…)]` is acceptable.** It is used (`too_many_arguments`) without a written
  policy.
- **The `Result` type of today's compute ports.** The ports in
  `delonix-compute/src/ports.rs` return `delonix_runtime_core::Result`, so the adapters that
  implement them (`HostWorkload`, `HostNetwork`, …) import the shared result type, and those imports
  count towards `shared_error_imports`. P3 decides errors per crate. No document says how a port's
  signature changes, and adding such an import in a new file fails the ratchet. If your change
  needs one, raise it in the PR. Don't work around the regex.
- **Crate names during the transition.** ADR-0040 D2 (still Proposed) sets the target names, but a brand-new crate
  that lands before its context exists (for example a second provider before the
  `delonix-provider-*` renames) has no written rule. Ask in the issue first.
- **Required doc comments.** There is no `missing_docs` lint, only the observed habit.
- **The fitness test for provider-name matching** (ADR-0040 D3 rule 3) is proposed but not yet
  implemented.

---

## Review checklist

Before you open the PR, go through the list:

1. `cargo fmt --all --check` and `cargo clippy --workspace --all-targets --locked -- -D warnings`
   are clean. → [§1](#1-the-tooling-that-enforces-style)
2. `python3 scripts/lang_ratchet.py` and `python3 scripts/arch_fitness.py` pass, and any baseline
   you lowered is in this commit. → [§1](#1-the-tooling-that-enforces-style)
3. New identifiers, comments, messages **and test names** are English. User text goes through
   `po::t`/`po::tf`, with `pt.po` entries. No `num`. → [§2](#2-language-of-the-code),
   [§3.4](#34-test-names)
4. The code is in the right crate and layer, versions are only in the root, the library has no
   `println!` and doesn't re-run its own binary. → [§4](#4-structure-where-code-goes)
5. Decisions are pure functions with unit tests, and I/O stays at the edges. →
   [§4.3](#43-pure-core-io-at-the-edges)
6. New backends implement a port, with no provider-name matching. Adapters don't depend on
   adapters. → [§5.1](#51-ports-and-adapters)
7. Adapter/provider failures use a crate `Error` that converts into `delonix_model::Error`. →
   [§5.2](#52-errors-per-crate-adr-0040-p3)
8. If you touched `proto/`: `scripts/contract_gate.py` passes, with one request per RPC, explicit
   identity, and a regenerated OpenAPI. → [§5.4](#54-the-node-contract)
9. No consumer name anywhere in `crates/`, `bins/` or `proto/`, comments included. →
   [§5.5](#55-the-engine-knows-no-consumer)
10. Nothing is accepted and then ignored. Errors state the fact and then the fix, and use the
    variant of the right class. → [§6](#6-error-handling-and-messages), [§3.8](#38-exit-codes-and-dx_-codes)
11. Every `unsafe` block has a `// SAFETY:` comment. There is no raw fork in a multi-threaded
    process, temp files use `write_private_temp`, and secrets use `write_atomic_mode`. →
    [§7](#7-unsafe-syscalls-and-processes)
12. Record mutations go through `update`, and new record fields have `#[serde(default)]`. →
    [§8](#8-state-and-concurrency)
13. Tests use isolated roots, the regression test fails with the fix reverted, and a measured
    number carries its date. → [§9](#9-tests)
14. CLI changes: all entry points are wired, verbs are Docker-aligned, cuts have no aliases, and
    the CLI baseline is updated. → [§3.5](#35-cli-commands-and-flags)
15. Kinds and manifest fields: a row in `kinds.rs`, `camelCase` fields with old spellings as
    aliases, and the schema regenerated. → [§3.6](#36-kinds-api-groups-and-manifest-fields)
16. ADR, `AGENTS.md` and the generated docs are updated if a boundary moved. →
    [§10](#10-comments-and-documentation)
