# ADR-0041: The node-local contract the control plane's agent consumes — coverage, promise, lifetime

- **Status:** Proposed (2026-09-15)
- **Date:** 2026-09-15
- **Deciders:** Walter (owner)
- **Depends on:** ADR-0040 (Proposed, PR #319) — its D4 chooses the encoding
  (`proto/delonix/node/v1`, gRPC + generated REST on the same unix socket). This ADR
  **does not re-decide the encoding**; it decides what the contract must cover before the
  agent can rely on it, what is promised and when, and how the server lives without a
  daemon.
- **Complements, does not reopen:** ADR-0010 (remote management API, **Rejected** — stays
  rejected). **Articulates with:** ADR-0002 (Phase 2b), ADR-0003 (capability model),
  ADR-0025 (MCP), ADR-0034 (the daemon question), `ARCHITECTURE.md` ADR-1 (daemonless).
- **Requested by:** `delonix-paas` ADR 0037 (accepted 2026-09-15, branch
  `arch/adr-0037-control-plane`, PR angolardevops/delonix-paas#493), requests R1–R4.

## Context

The control plane fixed this chain in its ADR 0037:

```
control plane ──gRPC, network, mTLS──► node agent (control-plane code, one per node)
                                           │  local contract, unix socket
                                           ▼
                                      delonix-runtime (this engine)
```

and asked the engine for four things: **R1** a stable, published local contract with a
stability promise and socket activation, inside guardrails #1 and #2; **R2** a measured
plan for the gaps the agent needs that only the CLI has; **R3** mutations served by
library calls instead of shelling out to `delonix`; **R4** fix the `delonix-mgmt` doc
that cites a `RemoteRuntime` that does not exist, and record that the `delonix` binary
name belongs to the engine.

ADR-0040 (written the same day, from the same PaaS plan) already answers most of R1 and
R3 structurally: one contract in `.proto`, local only, socket-activated, `delonix-mgmt`
deprecated on arrival, the application layer moved out of `delonix-runtime-bin` into
context crates (P2), the CRI calling use cases in-process (D5). What it leaves open is
exactly what the agent blocks on: **coverage** (its draft proto was written from the
engine's resource list, not from the agent's needs), **the stability tier of each
service** («What this ADR does not decide»), **process lifetime under a permanently
connected client**, and **authorization at the socket** (ADR-0003 is not mentioned).

**Measured on `origin/main` `a6b68bee` (workspace version 3.1.0), 2026-09-15.** Every
`path:line` below is at that commit.

### The current local surfaces, as they are

- **`delonix-mgmt`**: 40 route handlers (`crates/interfaces/delonix-mgmt/src/lib.rs:185-264`). **12
  shell out** to the `delonix` binary through one spawn site, `run_cli`
  (`lib.rs:490-509`, `bin = current_exe()` at `:50`): `delete_container`,
  `container_action_ep`, `container_logs_ep`, `container_exec_ep`, `run_container`,
  `pull_image` (+ optional scan), `build_image`, `scan_image`, `create_network`,
  `delete_network`, `reconfig_container`, `vm_action_ep`. **27 call libraries** (volumes,
  container/image reads, SBOM, every `delonix_net::infra` route, `/metrics`, `/v1/dash`).
  Mutations return `{ok, output}` — the CLI's stdout.
- **`delonix-mcp`**: stdio (ADR-0025); its one mutation and `logs.query` shell out via
  `run_cli_blocking` (`crates/interfaces/delonix-mcp/src/lib.rs:822-835`).
- **`delonix-cri`**: every pod/container lifecycle call forks the CLI
  (`Command::new(delonix_bin())`, `runtime_svc/lifecycle.rs:367`, `:401`), including
  `ExecSync` (`:1517-1535`) and the streaming exec/attach (`streaming.rs:395`, `:517`;
  `spdy.rs:546`, `:598`).
- **Docker API shim**: re-execs itself as `__apirun` (`cmd/dockerapi.rs:103-107`).
- **Socket activation: none.** `LISTEN_FDS`/`sd_listen`/`listenfd` — zero occurrences;
  no `*.socket` file is tracked. The only unit, `dist/delonix-cri.service`, is
  `Type=simple`, `Restart=always`, no `Sockets=` (lines 8, 19-20). There is no unit for
  `serve api` at all. (ADR-0040 D4 says the node API is socket-activated «exactly as the
  CRI unit does today» — that premise is wrong at `a6b68bee`; see Consequences.)
- **Authentication**: `SO_PEERCRED`, peer euid must equal the server's
  (`delonix-runtime-core/src/peer_cred.rs:18-39`; mgmt `lib.rs:136`, `:157-159`). The
  socket is `chmod 0600` **after** `bind` (`lib.rs:76`), a short window at umask mode; the
  peer check still runs on every connection.
- **`cli-stability.md:295-300`**: `serve api` is «NÃO estável», local, «não tem contrato
  publicado: não construas automação sobre ela».
- **`RemoteRuntime` / `InProcessRuntime`**: zero definitions in either repository.
  `git log --all -S RemoteRuntime` in `delonix-paas` finds only the commit of its ADR 0037,
  which quotes this engine's doc. The name entered `delonix-mgmt`'s doc with the crate
  (`87ce05ac`, 2026-07-19) and was copied into `delonix-security-runtime/src/lib.rs:43`,
  ADR-0010 (line 60) and ADR-0025 (lines 51, 108). ADR-0010's framing — «the socket keeps
  serving its real consumer» — was premised on a consumer that was never built.

### R2 — the coverage matrix

Legend: **LIB** = callable from a library crate · **CLI** = only in
`bins/delonix-runtime-bin` (no `[lib]`, `Cargo.toml:10-12`) · **API** = a local server
exposes it, and whether by library call or shell-out · **—** = nowhere. «0040 proto» =
the draft on `integra/adr-0040`.

| # | Capability | LIB | CLI | Local API today | 0040 proto |
|---|---|---|---|---|---|
| 1 | **VM create** | `delonix_vm::create` `delonix-vm/src/lib.rs:3673`, `create_with` `:3741` — takes a flat `VmConfig` (`:36`), not a manifest | `vm create` `cmd/vm.rs:494`; `vm apply -f` → `apply` `:1429` (manifest → `VmConfig` via `vm_spec_of` `:292`, CLI-only) | **—** (mgmt `/v1/vms/:name/action` accepts only `stop`/`rm`, `lib.rs:1501-1506`, shell-out) | `CreateVirtualMachine` |
| 2 | **VM start** | `delonix_vm::start` `lib.rs:4548`, `restart` `:4560` | `vm start` `cmd/vm.rs:776` | **—** (mgmt comment claimed «the runtime has no `vm start`» — stale, corrected in this PR) | `StartVirtualMachine` |
| 3 | **VM console** | path helpers only: `console_socket` `lib.rs:1730`, `serial_log_path` `:1741`; no console on `VmBackend` (`:760-890`) | `vm console [-e]` → `cmd_console` `cmd/vm.rs:3146` (libvirt: `virsh console`; CH: `console_bridge` `:3367`); `vm vnc` prints an address | **—** | `Console` (bidi stream) |
| 4 | **VM snapshot** create/ls/restore/rm | `delonix_vm::snapshot` `:4140`, `restore` `:4150`, `snapshots` `:4367`, `delete_snapshot` `:4373`; libvirt, CH and Proxmox implement them | `vm snapshot {create,ls,rm,restore}` `cmd/vm.rs:874`, thin wrapper | **—** | 4 RPCs |
| 5 | **stack plan / apply / destroy** | **—** (no planner in any lib) | `stack` `cmd/stack.rs:295`: `build_plan` `:458`, `apply` `:1368`, `destroy` `:1890`; `reconcile::plan` `cmd/reconcile.rs:348`; `manifest::load` `cmd/manifest.rs:490` | **—** | `PlanStack`, `ApplyStack`, `DestroyStack` |
| 5b | **stack history / rollback** (ADR-0019) | **—** | `stack history`, `stack rollback` (`cmd/stack.rs:295`) | **—** | **absent** |
| 6 | **HTTPRoute / Ingress** | **—** (`delonix-net` has port publishing, not routes) | `net httproute {apply,rm}` → `cmd/httproute.rs:167`, `apply` `:1043`; `Ingress` lowers to HTTPRoute; the L7 proxy is `ingress-proxy`, spawned by `ensure_running` `cmd/ingress_proxy.rs:886` via `current_exe` `:1000` | **—** | **absent** (no Gateway/HTTPRoute service) |
| 6b | **tunnel** (Gateway to pinggy/ngrok/cloudflare) | **—** | `net tunnel` → `cmd/tunnel.rs:193`, spawns the provider binary `:656` | **—** | **absent** |
| 7 | **image push** | `delonix_image::push_to_registry` `delonix-image/src/registry.rs:1268` | `image push` → `cmd_push` `cmd/image.rs:941`, thin wrapper | **—** (not in mgmt, not in the Docker shim matrix nor its refused list) | `PushImage` |
| 8 | **pod sandbox outside the CRI** | **—** (`create_pod` `cmd/pod.rs:305`, `remove_pod` `:459` are bin-private; the CRI's `run_pod_sandbox` `runtime_svc/lifecycle.rs:496` is `pub` inside a private module, `delonix-cri/src/lib.rs:28`) | `pod create -f` `cmd/pod.rs:113`; `delete pod` `cmd/verbs.rs:407`; **no pod stop anywhere** | **—** in mgmt; CRI only, by shell-out | `CreatePod/Get/List/Delete`, no stop |
| 9 | **exec, streaming with TTY** | `delonix_runtime::exec_with` `delonix-runtime/src/lib.rs:6235` (blocks in `waitpid`); the CRI's WebSocket v4/v5 + SPDY server is a `pub` lib (`delonix-cri/src/lib.rs:29-30`, `streaming.rs:72-165`, `spdy.rs:265`) but hard-codes `cri-{id}` (`streaming.rs:293`) and spawns the CLI | `container exec [-i] [-t]` → `cmd_exec` `cmd/container.rs:5987` | mgmt `POST /v1/containers/:id/exec` (`lib.rs:605-624`): **synchronous, no TTY, null stdin, no stream, no exit code, no timeout**, body `{cmd}` run as `sh -c`, shell-out | `Exec` (bidi stream) |
| 10 | **logs, follow** | **—** (only the private writer `log_shim` `delonix-runtime/src/lib.rs:916`; no public read/follow) | `container logs [-f] [--tail] [--since]` → `cmd_logs` `cmd/container.rs:7196` (300 ms poll, reopens on rotation) | mgmt `GET /v1/containers/:id/logs` (`lib.rs:581-592`): **one-shot, no follow, no tail**, shell-out; MCP `logs.query` shell-out | `Logs` (server stream) |
| 11 | **`cluster kubeadm` as an operation** | VM creation only (`delonix_vm::create`, called from `cmd/cluster.rs:2183-2223`) | `cluster kubeadm` `cmd/cluster.rs:589-649` → `provision_and_apply`: one blocking call, progress is a terminal spinner (`:2051`), kubeadm over SSH (`:2271-2331`), **no persisted progress, no events**; **no destroy for kubeadm clusters** (only kind, `:817-819`); no separate join | **—** | **absent** |
| 12 | **node capacity** | `resource_advice::collect` `delonix-runtime/src/resource_advice.rs:606` (CPU, memory, swap, disk via statvfs, PSI, delegated controllers); usage in `dashstats` `delonix-mgmt/src/dashstats.rs:25-60` | `system resources` `cmd/system.rs:1369` | mgmt `/metrics`, `/v1/dash` (lib); MCP `resources.get` (lib) — **no single allocatable-vs-used view** | `GetCapacity` |
| 13 | **node health** (dependencies) | store check in MCP `doctor_checks` `delonix-mcp/src/lib.rs:773-818`; CRI `Status` — `RuntimeReady` **always true** (`delonix-cri/src/runtime_svc.rs:123`), `NetworkReady` real | `system doctor` `cmd/system.rs:2037-2150` (br_netfilter, cgroup2 delegation, subuid, `slirp4netns`/`nft`/`ip`) — **checks are bin-private**; no `max_user_namespaces` check | mgmt `/v1/net/status` (lib); nothing aggregates health | `GetHealth` |
| 14 | **events, watch** | `delonix-runtime-core/src/events.rs`: append-only `events.jsonl`, rotates at 4 MiB, `read_from(offset)` `:139` — resumable | `system events -f` (1 s poll) `cmd/system.rs:941-985`; writers are almost all in the CLI; **VM and cluster emit none** | **—** (Docker `/events` refused, `dockerapi.rs:481`) | `WatchEvents` |

**Found while measuring, out of this ADR's scope — fixed since, in #322** (squash
`9669dba8`, 2026-09-15): at `a6b68bee`, in root mode without CNI, the CRI's
`RunPodSandbox` ran `delonix pod create <pod> --network` (`runtime_svc/lifecycle.rs:551`)
and `RemovePodSandbox` ran `delonix pod rm` (`:683`, result discarded), while `PodCmd`
accepted only `create -f` and had no `rm` (`cmd/pod.rs:111-130`). #322 moved root
sandboxes onto CNI on the host (a named netns per sandbox, CNI DEL on removal) and
removed both argv forms; its author proved it on a kubeadm VM (node Ready, CoreDNS,
Service by ClusterIP, netns and lease released on pod deletion) — measured there, not by
this ADR. The lesson stays: this is the class of bug R3 exists to remove, an argv
contract between two crates that no compiler checks, and it survived because the
published CRI numbers are rootless-only.

### Guardrails this decision touches

#1 daemonless (a permanently connected agent) · #2 no tenant (the agent translates
identity; nothing crosses) · #5 no new privilege boundary — but a new *caller* of the
highest-privilege socket · #6 no silent failure (unmeasured capacity, unsupported RPCs).

## Decision

### D1. The consumer is named, and what that triggers — and does not

The node agent of `delonix-paas` ADR 0037 is a **concrete, local consumer** with written
requirements. Recording its effect on earlier decisions, so no later session re-argues
them:

- **ADR-0010 is not reopened.** Its reopening condition is «a consumer that is neither the
  PaaS nor a local agent». This one is both. Transport stays a unix socket; identity,
  mTLS and audit of remote callers live in the agent («the remoteness belongs one layer
  up» — now built, one layer up).
- **ADR-0002 Phase 2b is triggered** (a second consumer beyond the CLI). ADR-0040 already
  records this; it is carried out there (context crates), not here.
- **ADR-0003's trigger is met.** ADR-0003 waited for «a lower-trust local socket
  consumer». A process on the node that terminates network mTLS from the control plane is
  exactly that: compromising the agent today yields everything the engine's uid can do,
  including `exec` into every container. Therefore:
  - every RPC of the node contract declares the ADR-0003 `Capability` it requires, as a
    proto method option; a contract test fails if an RPC has none;
  - the gate is enforced once, at the node API's dispatch, after `SO_PEERCRED`;
  - the policy is node-local and default-all (behaviour unchanged when unused), never
    carried on the wire;
  - ADR-0003 itself moves from «wait» to «implement with the node API» — **its own
    acceptance** remains a separate owner decision; this ADR only removes its blocker.
- **Same-euid stays the authentication.** The agent runs as the engine's uid. A peer-uid
  allowlist (agent as a separate uid) is a change to the socket's trust model and is
  ADR-0003 successor material, not decided here.

### D2. Coverage — what the contract must serve before the agent may depend on it

Each gap in the matrix gets one of the three states the engine already uses for
compatibility (served / refused with a written reason / missing), and the contract is
closed in **waves ordered by what the agent's strangler order needs** (its ADR 0037 D5
cuts ports in the order container, pod, volume, ingress, tunnel, secret):

| Wave | Contract surface | Gap rows | Precondition in the engine |
|---|---|---|---|
| **W1** | Containers incl. `Exec` (TTY, stdin, resize, exit code) and `Logs` (follow, tail, since); `WatchEvents`; `GetHealth`; `GetCapacity`; `Operations` | 9, 10, 12, 13, 14 | a public log reader/follower in a lib (row 10 has none); the streaming server generalised from `cri-{id}`; health checks moved out of `cmd/system.rs`; health reports each unmeasured dependency as **unmeasured**, never ok (#6); CRI `RuntimeReady` stops being constant |
| **W2** | Images incl. `PushImage`; volumes; networks | 7 | none beyond ADR-0040 P2 — push, volumes and network reads are already library calls |
| **W3** | VMs: create from a spec, start, stop, delete, `Console`, snapshots | 1–4 | manifest → `VmConfig` moves out of `cmd/vm.rs`; console becomes a `VmBackend` capability (ADR-0040 D3), not a `virsh` exec in the CLI; **VM lifecycle emits events** |
| **W4** | Stacks: plan, apply, destroy, **history, rollback** | 5, 5b | the planner lands in `delonix-stack` (ADR-0040 P2); history/rollback added to the proto |
| **W5** | **`GatewayService`**: HTTPRoute/Ingress get/list/apply/delete | 6 | added to the proto (absent today); the L7 proxy's lifetime is stated by the contract (it is a spawned process today) |
| **W6** | Pods outside the CRI, including **stop** | 8 | pod stop exists in the engine before it exists in the contract |
| **W7** | **`ClusterService`**: kubeadm create as an `Operation` with persisted progress | 11 | **a kubeadm destroy exists** — an API that creates what it cannot delete leaks by construction; progress written to the operation record, not a spinner |
| — | **tunnel** | 6b | **refused with reason** for now: it drives third-party binaries and accounts (pinggy/ngrok/cloudflare) — the agent asked for no tunnel RPC in R2, and a public tunnel opened from the node is a control-plane decision. Reopens with a named need |
| — | **secrets** | — | **not measured here**: the agent's port list names a secret port, R2 did not. A separate question — whether platform secrets should reach the node store at all is `ngolacloud-cyber-resilience` material |

Two rules apply to every wave:

1. **An RPC the proto declares but the server does not serve returns `UNIMPLEMENTED` with
   a reason**, and appears as «missing» in a generated coverage table (the
   `API_MATRIX` pattern, `cmd/dockerapi.rs`). Never an empty success.
2. **A wave is closed by the agent's contract test passing against a real engine**, not by
   the route answering 200 — the same bar ADR-0037 D6 of the control plane sets for its
   strangler cuts.

### D3. R3 — «served» means a library call

- **An RPC is marked `served` in the coverage table only when its handler calls a
  library.** A handler that shells out to `delonix` is `preview`, and the table says so.
  The 12 mgmt shell-outs, the MCP pair and the CRI's argv builders are the inventory; each
  migrates when its context crate lands in ADR-0040 P2, in the wave order above.
- **The one justified process boundary stays**: `clone` into new namespaces from a
  multi-threaded tokio process is unsafe (`dockerapi.rs:133-138`, `spdy.rs:209-212`). It
  moves behind ADR-0040's `ProcessLauncher` with a typed spec over an fd — never an argv.
  Exec and log streaming therefore reach a lib call for everything except the final spawn.
- This is a phase of ADR-0040 P2/P5, not a separate plan; no new crate is introduced here.

### D4. R1 — the promise, and when it starts

- **No promise before the gate.** The contract is not stable while it is a draft. To stop
  a promise being made by a package name, the draft package is **`delonix.node.v1alpha1`**
  (the ADR-0040 draft says `v1` — this ADR asks it to change) until W1 closes.
- **At W1**: a «Node contract» section in `docs/cli-stability.md` promises, per service:
  `v1` services only grow additively within the major (`buf breaking` against the last
  tag, in CI); a removal needs `v2` served alongside `v1` for at least one minor release;
  `UNIMPLEMENTED`-with-reason RPCs carry no promise beyond their shape. Services join the
  promise one at a time as their wave closes — a stack service at `v1alpha1` next to a
  container service at `v1` is the honest state.
- **`delonix-mgmt` is frozen now**: bug and security fixes only, no new routes. Every gap
  the agent needs is added to the node contract, not to these routes — widening them
  would build exactly the translation layer ADR-0040 removes. It is removed when the agent
  has migrated (ADR-0040 D4).
- **The binary name `delonix` is the engine's** (`bins/delonix-runtime-bin/Cargo.toml:11`).
  The control plane stops producing an executable of that name (its ADR 0038). Recorded
  in `docs/cli-stability.md` in this PR.

### D5. Lifetime — socket activation, with the permanent client written down

- The node API is started by a systemd **socket unit** (`Accept=no`, one service
  instance), receives the listener via `LISTEN_FDS`, and **exits after an idle interval
  with no open stream and no operation in `RUNNING`**. Operations are persisted under the
  state root before they are acknowledged (ADR-0040 D4); a restart ends an interrupted one
  as `FAILED/Interrupted`.
- **The honest part.** An agent holding `WatchEvents` open keeps the server alive for as
  long as the agent runs — in practice, always. Guardrail #1 (and `ARCHITECTURE.md`
  ADR-1) says no resident process is **required**; it does not say a process may never be
  long-lived. The line this ADR draws:
  - nothing in the engine depends on the server being up: every operation is persisted,
    `events.jsonl` is written by whoever acts, and `WatchEvents` resumes from an offset
    (`events.rs:139`) — so the server can be killed at any moment and lose nothing;
  - the agent **must** tolerate the server exiting and reconnect with its last offset;
    that is part of the contract test;
  - long-lived because a client chose to stay connected ≠ a daemon the engine needs. If a
    requirement ever appears that only a process the engine keeps alive can satisfy
    (in-memory state, timers the engine owns), that is the daemon ADR that ADR-0034 and
    guardrail #1 already demand — not this one.
- **Spike before code** (GO/NO-GO, on the golden `delonix-vm-base:ubuntu-24.04`): socket
  activation hands the listener to the server; idle exit happens; a client reconnecting
  during exit is served by the next activation; an operation cut by `kill -9` ends
  `Interrupted`; the `SO_PEERCRED` check holds on an activated socket, and the unit
  creates the socket `0600` so the chmod-after-bind window disappears.

## Alternatives considered

- **Fold all of this into ADR-0040.** Legitimate, and the owner may prefer it. Kept
  separate because ADR-0040 is a 170k-line restructuring whose acceptance may take a
  while, and the control plane's F2/F3 block on coverage and promise, not on the crate
  map. If ADR-0040 is rejected, this ADR's D2 matrix, D4 and D5 still stand and apply to
  whatever encoding replaces it.
- **Publish an OpenAPI for `delonix-mgmt` as the contract.** Rejected: 12 of 40 handlers
  return CLI stdout, exec/logs cannot stream, and a contract describing those routes would
  promise the shell-out. ADR-0040 already chose the proto; the choice R1 left to the engine
  is taken there.
- **Widen `delonix-mgmt` now, gap by gap, for the agent.** Rejected: fastest to the first
  VM create, and it fixes the wrong shape in place — every route added is another argv
  builder to migrate and another surface to deprecate.
- **Keep the control plane linking engine crates.** Rejected by the control plane itself
  (its ADR 0037: 7 000 copied lines, libraries with no stability promise). Nothing for the
  engine to decide.
- **A resident `delonix-node-api` (`Restart=always`, like the CRI unit).** Rejected here:
  guardrail #1, and the persisted-operation + resumable-events design removes the need.
  The CRI unit's current shape is not a precedent for this server.
- **TCP + mTLS in the engine.** Rejected, ADR-0010.
- **Expose tunnels in W5.** Rejected for now — see the D2 row; no consumer asked, and the
  decision to open a public tunnel belongs above the engine.

## Consequences

**Easier:** the agent has a written order of what arrives when, and a test that closes
each wave; ADR-0003 gets the concrete consumer it waited for; the coverage table makes
«preview» (shell-out) visible instead of letting it pass as served; a server that can die
at any time is simpler to operate than one that must not.

**Harder, in order of risk:**

1. **Row 10 and row 13 need engine work before any RPC**: there is no library log
   follower and the health checks are bin-private. W1 is not «wire the proto».
2. **The agent runs as the engine's uid** with the full power of the socket until
   ADR-0003's gate exists. W1 without the capability gate is a remote-code-execution path
   one mTLS bug away; the gate is in W1, not later.
3. **Two premises of ADR-0040 need correcting there**: the CRI unit is not socket-activated
   (`dist/delonix-cri.service`: `Type=simple`, `Restart=always`), and the draft proto lacks
   `GatewayService`, `ClusterService`, stack history/rollback and pod stop, and uses `v1`.
4. **Freezing `delonix-mgmt`** means a gap found before W-n closes waits for the contract,
   even when a route would be ten lines.
5. **W7 depends on a kubeadm destroy** that does not exist; cluster exposure may be last
   by a long way.

**Guardrail audit:** #1 socket-activated, idle-exit, nothing depends on the process being
up; a permanently connected client is written down, not hidden ✅ · #2 no tenant, quota or
licence in the contract; identity translated in the agent ✅ · #3 no private dependency ✅ ·
#4 no new crate or dependency in this ADR ✅ · #5 no new privilege boundary; a new caller of
the existing one, gated by ADR-0003 in W1; spike in D5 before code ✅ · #6
`UNIMPLEMENTED`-with-reason, unmeasured-as-unmeasured, shell-out marked `preview` ✅.

## What changed in this PR besides the ADR (R4)

- `crates/interfaces/delonix-mgmt/src/lib.rs`: the module doc no longer names `RemoteRuntime`; the
  three `InProcessRuntime` references are replaced by what is true; the stale «the runtime
  has no `vm start`» comments now point at `delonix_vm::start`. Comments only.
- `crates/contexts/delonix-security-runtime/src/lib.rs:43`: same `RemoteRuntime` reference.
- ADR-0010 and ADR-0025 keep their text (accepted ADRs are not rewritten); this ADR is the
  record that the consumer they named did not exist.
- `docs/cli-stability.md`: the `serve api` bullet records the freeze and that `delonix` is
  the engine's binary name.

## Proven vs not validated

**Proven** (read at `a6b68bee`, references above): the 40/12/27 handler split and the
single spawn site; every row of the matrix; zero socket activation and the CRI unit's
shape; same-euid peer check and chmod-after-bind; `RemoteRuntime`/`InProcessRuntime` exist
in neither repository; the `events.jsonl` offset reader; the ADR-0040 draft proto's service
list.

**Not validated:** nothing was built or run for this ADR; the root-mode CRI pod argv bug
was read from code here and fixed and proven live elsewhere (#322), not by this ADR; socket activation, idle exit and reconnect are designed,
not spiked (D5); whether `delonix_volume` and `delonix_net::infra` spawn processes
internally was not opened (their mgmt handlers do not); whether MCP `network.inspect`
includes HTTPRoute state was not confirmed; the agent's actual RPC call pattern and
latency are unknown until the control plane writes its contract test; the ordering of
waves assumes the control plane's ADR 0037 D5 port order holds.
