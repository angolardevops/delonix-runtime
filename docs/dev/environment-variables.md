# Environment variables (`DELONIX_*`)

**Before you read:** [Isolating the engine's state](build-and-test.md#isolating-the-engines-state) in Clone, build and test.

This page lists every `DELONIX_*` name that appears in the engine's code, with where it is read,
what it changes, and whether you should ever set it. It is a reference: read the section that
matches what you are doing, not the whole page. After it you can isolate a run, turn on the
diagnostics you need, and recognise the variables that lower a security boundary before you set one.

## How to read this page

Each row of the variable tables has five columns:

- **Read by** — the crate or binary and the `path:symbol` where the value is read (or written).
  Paths are relative to the repository root.
- **Purpose** — what the variable changes.
- **Values / default** — how the code parses it. When the code falls back silently on a value it
  cannot parse, the row says so.
- **Notes** — when to use it, and any warning.

Most variables are read by the process that needs them, **not** passed on automatically. A few are
set by the engine itself on the processes it starts; those are in
[Set by the engine itself](#set-by-the-engine-itself-internal) and you should not set them.

**This table is checked by CI in both directions.** `python3 scripts/dev_docs.py --check`
(function `env_var_problems` in `scripts/dev_docs.py`) extracts every `DELONIX_*` name from Rust
string literals under `crates/` and `bins/` (including `build.rs` and `env!()`) and from
`scripts/install.sh`, and fails when a name is missing here or when this page lists a name the code
no longer contains. A row counts only when its line starts with `` | `DELONIX_NAME` ``. The
extraction is textual, so it also picks up a few names that are **not** environment variables (a
Rust constant, test fixtures, a key written into a VM image); those are listed in their own
sections so the check stays exact.

Variables used only by scripts outside that scope — for example `DELONIX_CHAOS_DIR` and the
`DELONIX_CHAOS_TRUENAS_*` family in `scripts/chaos.sh` — are documented in the header of each
script and in [Clone, build and test](build-and-test.md), not here.

### Precedence

Where the code has a precedence rule it is usually **flag > environment > default**; the table shows each case as the code resolves it, including the ones with an extra level or no flag:

| Setting | Order (first wins) | Where |
|---|---|---|
| CRI socket, capability ceiling and mode | `--addr` / `--cap-ceiling` / `--cap-ceiling-mode` > `DELONIX_CRI_ADDR` / `DELONIX_CRI_CAP_CEILING` / `DELONIX_CRI_CAP_CEILING_MODE` > `unix:///run/delonix-cri.sock` / no ceiling / `reject` | `crates/interfaces/delonix-cri/src/bin/delonix-cri.rs:main` |
| Management API socket | `--addr` > `DELONIX_API_ADDR` > `unix:///run/delonix-mgmt.sock` | `bins/delonix-mgmt-bin/src/main.rs:run` |
| Docker API socket | `--addr` > `DELONIX_DOCKER_ADDR` > `unix:///run/delonix-docker.sock` | `bins/delonix-runtime-bin/src/cmd/dockerapi.rs:run` |
| Output language | `--l18n` > `DELONIX_L18N` > English | `bins/delonix-runtime-bin/src/cmd/po.rs:peek_lang` |
| VM console escape key | `--escape` > `DELONIX_CONSOLE_ESCAPE` > `^]` | `bins/delonix-runtime-bin/src/cmd/vm.rs:resolve_escape` |
| VM backend | `--backend` (or the image's `HYPERVISOR`) > `DELONIX_VM_BACKEND` > `vm default-backend --set` > auto-detection | `crates/adapters/delonix-vm/src/lib.rs:standing_backend_choice` |
| Published port bind address | the address in `-p <ip>:<host>:<container>` > `DELONIX_PUBLISH_ADDR` > `127.0.0.1` | `crates/adapters/delonix-sdn/src/lib.rs:publish_bind_addr` |
| CVE feed for `image scan --update` | `--feed` > `DELONIX_ADVISORY_FEED` > error | `bins/delonix-runtime-bin/src/cmd/scan.rs:cmd_scan_update` |
| Log filter | `DELONIX_LOG` > `RUST_LOG` > `info` | `crates/adapters/delonix-telemetry/src/telemetry.rs:init` |

`delonix serve cri` and `delonix serve api` forward their flags **both** as flags and as the
matching variable to the server binary they `exec` (`bins/delonix-runtime-bin/src/cmd/serve.rs:run`),
so a server from an older release that only reads the variables still gets the value.

### Isolating a development run

Before running anything beyond `--help` on a machine that also runs Delonix workloads, point both
state locations at a scratch directory:

```bash
export DELONIX_ROOT=$HOME/scratch/dlx/root        # records, images, networks, IPAM, volumes, pidfiles
export DELONIX_NET_RUNTIME_DIR=/tmp/dlx-run        # the network holder's control and slirp sockets
mkdir -p "$DELONIX_ROOT" "$DELONIX_NET_RUNTIME_DIR"
./target/debug/delonix system info                # "state root" must show your scratch path
```

**Why both.** The network infrastructure keeps its pidfiles under `DELONIX_ROOT` but its unix
sockets in a separate runtime directory (`crates/adapters/delonix-sdn/src/infra.rs:runtime_dir`),
because a socket path is limited to about 108 bytes and `DELONIX_ROOT` can be arbitrarily deep.
Setting only one of the two can leave two state roots sharing one set of sockets. The full story,
and how to tear the isolated infra down, is in
[Isolating the engine's state](build-and-test.md#isolating-the-engines-state).

## Everyday configuration

| Variable | Read by | Purpose | Values / default | Notes |
|---|---|---|---|---|
| `DELONIX_PERF_CONF` | `scripts/install.sh` (the generated `/usr/local/sbin/delonix-performance`) | Path of the config (`CPU=`/`THP=` flags) the helper reads. | A file path. Unset: `/etc/delonix/performance.conf`. | Only for testing the helper against a fake sysfs; `install.sh` writes the real one. |
| `DELONIX_PERF_CPU` | `scripts/install.sh` (the generated `/usr/local/sbin/delonix-performance`) | Root of the CPU sysfs tree the helper reads and writes (governor, EPP). | A directory path. Unset: `/sys/devices/system/cpu`. | Point it at a fake tree to test `apply`/`revert` without touching the host. |
| `DELONIX_PERF_STATE` | `scripts/install.sh` (the generated `/usr/local/sbin/delonix-performance`) | Directory where the helper stores the boot-time values it restores on `revert`. | A directory path. Unset: `/var/lib/delonix-performance`. | Test-only override, like the others. |
| `DELONIX_PERF_THP` | `scripts/install.sh` (the generated `/usr/local/sbin/delonix-performance`) | The transparent-hugepage `enabled` file the helper reads and writes. | A file path. Unset: `/sys/kernel/mm/transparent_hugepage/enabled`. | Test-only override, like the others. |
| `DELONIX_ROOT` | every store and binary: `bins/delonix-runtime-bin/src/cmd/util.rs:state_root`, `crates/adapters/delonix-oci/src/image.rs:ImageStore::default_root`, `crates/adapters/delonix-state/src/store.rs:Store::default_root`, `crates/adapters/delonix-sdn/src/infra.rs:base_root`, `crates/interfaces/delonix-cri/src/bin/delonix-cri.rs:main`, `bins/delonix-mgmt-bin/src/main.rs:run`, `crates/interfaces/delonix-mcp/src/lib.rs` | The engine's state root: container records, images, networks, IPAM, volumes, VMs, secrets. | A directory path. Unset: `$XDG_DATA_HOME/delonix` (or `~/.local/share/delonix`) when not root, `/var/lib/delonix` as root. **`delonix-cri` and `delonix-mgmt` default to `/var/lib/delonix` regardless**, which is why `delonix serve …` passes the CLI's root explicitly (`cmd/serve.rs:exec_server`). | The one variable to set when testing. The engine also **sets** it on every child it starts (re-exec passes, the network holder, servers, CRI lifecycle calls) so paths agree across user namespaces. |
| `DELONIX_NET_RUNTIME_DIR` | `crates/adapters/delonix-sdn/src/infra.rs:runtime_dir` (`RUNTIME_DIR_ENV`) | Directory for the network infrastructure's unix sockets (`control.sock`, `slirp.sock`). | A directory path; keep it short (socket paths over ~108 bytes fail with `SUN_LEN`). Unset: `/tmp/delonix-net-<uid>` plus a suffix derived from a non-default `DELONIX_ROOT`. | Set it together with `DELONIX_ROOT` when isolating. The engine also passes it to the holder and to `--net <custom>` re-exec passes (`infra::runtime_dir_env`), because inside the holder's user namespace the uid is 0 and the default would differ. |
| `DELONIX_L18N` | `bins/delonix-runtime-bin/src/cmd/po.rs:peek_lang` | Output and `--help` language of the CLI. | `en` (default) or `pt`. | `--l18n` wins. Error *classes* (exit codes) do not depend on the language, messages do — do not grep messages in scripts. |
| `DELONIX_LOG` | `crates/adapters/delonix-telemetry/src/telemetry.rs:init` | Log filter for `delonix`, `delonix-cri`, `delonix-mgmt` and `delonix-mcp`. | A `tracing` filter expression (`debug`, `warn`, `delonix_sdn=debug`). Falls back to `RUST_LOG`, then `info`. | Logs go to stderr; stdout is kept for command output. |
| `DELONIX_LOG_FORMAT` | `crates/adapters/delonix-telemetry/src/telemetry.rs:init` | Log line format. | `json` for JSON lines; anything else (or unset) is plain text. | Useful when a server runs under systemd and its journal is shipped somewhere. |
| `DELONIX_VERBOSE` | `bins/delonix-runtime-bin/src/cmd/output.rs` (`Progress`) | Streams every step's output instead of folding it into one progress line. | Set and not `0` → verbose. | Same effect as `--verbose` where a command has one. |
| `DELONIX_HOSTS_FILE` | `bins/delonix-runtime-bin/src/cmd/hosts_file.rs:hosts_path` | The hosts file that `hosts: [host]` on an `HTTPRoute` and `delonix hosts sync` write their managed block into. | A file path; default `/etc/hosts`. | Point it at a scratch file in an isolated run so the real `/etc/hosts` is never touched (writing it needs root). How the block works: [How names reach `/etc/hosts`](service-names-and-hosts.md). |
| `DELONIX_CONSOLE_ESCAPE` | `bins/delonix-runtime-bin/src/cmd/vm.rs:resolve_escape` | The key that detaches `delonix vm console`. | One control key as `^X` or `X`. Default `^]`. An invalid value is an error, not a fallback. | For keyboard layouts where `^]` cannot be typed (e.g. Portuguese). `-e/--escape` wins. |
| `DELONIX_CRI_ADDR` | `crates/interfaces/delonix-cri/src/bin/delonix-cri.rs:main`; forwarded by `bins/delonix-runtime-bin/src/cmd/serve.rs:run` | Socket the CRI server listens on. | `unix://<path>`. Default `unix:///run/delonix-cri.sock`. | `--addr` wins. The kubelet's `--container-runtime-endpoint` must match. |
| `DELONIX_CRI_CAP_CEILING` | `crates/interfaces/delonix-cri/src/bin/delonix-cri.rs:main` (`cap_ceiling::CEILING_ENV`, parsed by `crates/interfaces/delonix-cri/src/cap_ceiling.rs:CapCeiling::parse`); forwarded by `cmd/serve.rs:run` | Node-level upper bound on the capabilities of any container created through the CRI, `privileged: true` included. | Empty/unset or `all` → no ceiling (unchanged behaviour). `none` → no capabilities. `default` → the engine's default set. `default,NET_ADMIN,…` → the default plus the named ones. A list of names (`CAP_` prefix optional, case-insensitive; separated by commas, spaces or `;`) → exactly those. `all` anywhere in the list wins. An unknown name, or a value made only of separators, **refuses to start the server**. | `--cap-ceiling` wins. Bounds capabilities only: a privileged pod still gets unconfined seccomp and a writable `/sys`. The ceiling in force is visible in `crictl info` (`capabilityCeiling`). |
| `DELONIX_CRI_CAP_CEILING_MODE` | `crates/interfaces/delonix-cri/src/cap_ceiling.rs:CeilingMode::parse` (`MODE_ENV`); forwarded by `cmd/serve.rs:run` | What happens when a pod explicitly asks for more than the ceiling. | `reject` (default; also `enforce` or empty) → `CreateContainer` fails naming the denied capabilities. `clamp` (or `trim`) → reduced to the ceiling with a warning. An unknown word **refuses to start the server**. | `--cap-ceiling-mode` wins. In both modes the engine's implicit default set is reduced to the ceiling without an error. |
| `DELONIX_CRI_FROM_SOURCE` | `bins/delonix-runtime-bin/src/cmd/vmimage.rs:locate_cri_bin` (`CRI_FROM_SOURCE_ENV`, parsed by `cri_from_source_requested`) | Opt-in to compile `delonix-cri` from the source checkout around the cwd when `cluster apply` / `cluster kubeadm` need a binary to install on the nodes. | Only `1` or `true` (after trimming) turns it on; unset, empty, `0` or anything else is off. | Off by default so the runtime installed on a cluster never depends on the directory the command ran from. Without it the order is `--cri-bin`, the `delonix-cri` next to `delonix`, then the release asset of the running version, verified against its `SHA256SUMS`. The origin, path and sha256 are always printed. |
| `DELONIX_API_ADDR` | `bins/delonix-mgmt-bin/src/main.rs:run`; forwarded by `cmd/serve.rs:run` | Socket of the local management API (`delonix serve api`). | `unix://<path>`. Default `unix:///run/delonix-mgmt.sock`. | `--addr` wins. The API is local only (the calling uid). |
| `DELONIX_DOCKER_ADDR` | `bins/delonix-runtime-bin/src/cmd/dockerapi.rs:run` | Socket of the Docker Engine API slice (`delonix serve docker-api`). | `unix://<path>` (the `unix://` prefix is optional). Default `unix:///run/delonix-docker.sock`. | `--addr` wins. |
| `DELONIX_VM_BACKEND` | `crates/adapters/delonix-vm/src/lib.rs:standing_backend_choice` | Session-wide VM backend when a command does not name one. | A backend name (`libvirt`, `cloud-hypervisor`, or a registered remote backend such as `proxmox`). Blank is ignored. | Below `--backend` and the image's `HYPERVISOR`, above the machine default from `delonix vm default-backend --set`. Like an explicit choice, it overrides the capability heuristic and can fail late at boot if the backend cannot run the VM. |
| `DELONIX_NO_CGROUP_WARN` | `crates/adapters/delonix-linux/src/lib.rs` (the rootless-without-delegation warning and `warn_if_unprotected_memory`), `bins/delonix-runtime-bin/src/cmd/kindmode.rs` | Silences the warnings about missing cgroup delegation and about a container with no memory ceiling anywhere. | Set (any value) → silent. | The engine **sets** it itself (`cmd/util.rs:silence_cgroup_warning`, `cmd/kindmode.rs`) so that re-exec children do not repeat a warning the parent already printed. Setting it by hand hides a real condition: limits are not enforced. |
| `DELONIX_POLICY_LINT` | `bins/delonix-runtime-bin/src/cmd/policy.rs:show_lints` | Silences the one-per-command runtime-policy warnings (`warning: runtime policy [...]`). | `0` → silent; anything else or unset → shown. | For someone who has read the warning and decided otherwise. |
| `DELONIX_NO_AUTO_RECOVER` | `bins/delonix-runtime-bin/src/cmd/netns.rs:reconcile_after_respawn` | After the network holder is rebuilt, report the stranded containers and the command to restart them instead of restarting them automatically. | Set (any value) → report only. | For hosts where you want to choose when a database restarts. |
| `DELONIX_NO_AUTO_DELEGATE` | `bins/delonix-runtime-bin/src/cmd/kindmode.rs` (cgroup preflight of `cluster create`) | Disables the automatic re-exec of `cluster create` under `systemd-run --user --scope -p Delegate=yes` when the `cpu` controller is not delegated. | Set (any value) → print the error instead of re-executing. | For scripts and CI that prefer the plain error. |

## Networking and security escape hatches

**Every variable in this section lowers a boundary.** Each one logs a `SECURITY WARNING` (or a
warning) when it takes effect. They exist for debugging and for explicit, informed opt-outs; none of
them belongs in a production configuration. Read the linked `AGENTS.md` section before using one.

| Variable | Read by | Purpose | Values / default | Notes |
|---|---|---|---|---|
| `DELONIX_FORWARD_POLICY` | `crates/adapters/delonix-sdn/src/infra.rs` (ingress ruleset builder) | Reverts the holder netns `forward` chain from default-deny (`policy drop`) to default-allow. | `accept` → default-allow; anything else → default-deny. | **Lowers isolation between networks.** Logged as a security warning. See `AGENTS.md`, *«Bloco 0 do plano 33 (v0.37.1)»*. |
| `DELONIX_ALLOW_LINK_LOCAL` | `crates/adapters/delonix-sdn/src/infra.rs` (`fwguard` chain) | Removes the unconditional drop of `169.254.0.0/16` (cloud metadata) and `127.0.0.0/8` (host loopback) for container traffic. | `1` → allowed; anything else → dropped. | **Exposes instance metadata credentials on a cloud host.** Security warning. `AGENTS.md`, *«Bloco 0 do plano 33 (v0.37.1)»* (RF-NET-02). |
| `DELONIX_ALLOW_HOLDER_INGRESS` | `crates/adapters/delonix-sdn/src/infra.rs` (`dlxinput` chain) | Lets containers reach services the holder itself exposes (the L7 proxy, internal DNS) beyond the allowlist. | `1` → allowed; anything else → new connections dropped. | **Through the proxy a container reaches any registered backend, in any namespace, past that backend's own ingress policy.** Security warning. Rationale in `docs/discovery/46_GAPS_ENCONTRADOS.md` §4.2. |
| `DELONIX_ENABLE_IPV6` | `crates/adapters/delonix-sdn/src/infra.rs:ipv6_sdn_enabled` | Gives containers IPv6 addresses on the SDN again. | `1` → enabled; anything else → IPv6 disabled in the container and forwarding refused. | **No firewall rule, namespace isolation or `Dependency` applies to IPv6** — every policy is IPv4-only. Security warning. `AGENTS.md`, *«Bloco 0 do plano 33 (v0.37.1)»*. |
| `DELONIX_ALLOW_UNENFORCED_LIMITS` | `bins/delonix-runtime-bin/src/cmd/container.rs:preflight_resource_limits` | Runs a container with `-m`/`--cpus`/`--cpu-weight` even when this session has no cgroup delegation, instead of refusing (exit 69). | Set (any value) → run, unlimited, with a warning. | The kernel never sees the limits. See [cgroup delegation](environment.md#cgroup-delegation-some-limits-are-refused-others-are-not-enforced). |
| `DELONIX_INSECURE_BESTEFFORT` | `crates/adapters/delonix-linux/src/lib.rs:insecure_besteffort` | Skips the fail-closed confinement verification (seccomp, capabilities, `no_new_privs`) that runs before `execve` in a container's init and in `exec`. | Set (any value) → the check is skipped. | **A container may start with confinement that silently did not apply.** Read by the engine before the container's environment is applied, so a container cannot set it for itself. |
| `DELONIX_PUBLISH_ADDR` | `crates/adapters/delonix-sdn/src/lib.rs:publish_bind_addr`; suggested by `bins/delonix-runtime-bin/src/cmd/vm.rs` (`vm reach`) | Host address that published ports bind to when `-p` does not name one. | An IPv4 address; a value that is not IPv4 is ignored. Default `127.0.0.1`. | `0.0.0.0` exposes published ports on every interface. `vm reach` suggests the libvirt gateway so VMs can reach a container without exposing it to the LAN. `AGENTS.md`, *«Revisão do flow `-p` ↔ `ingress`/`egress` (2026-07-27)»*. |
| `DELONIX_SUBNET_BASE` | `crates/adapters/delonix-sdn/src/lib.rs:default_base` | Forces the second octet of the default network (`10.<base>.0.0/16`). | An integer 0–255 (no further range check). Unset: the value persisted in `<root>/net/default-base`, else a free octet detected on the host. | Only for a collision the detection did not see. Changing it after containers exist changes the default network's addresses. |
| `DELONIX_CNI` | `crates/adapters/delonix-sdn/src/cni.rs:enabled_conf`, consumed by `crates/interfaces/delonix-cri/src/runtime_svc/lifecycle.rs` (`RunPodSandbox`) and `runtime_svc.rs` (`UpdateRuntimeConfig`) | **Rootless CRI only:** networks pod sandboxes through the CNI plugin chain (`/etc/cni/net.d`, plugins from `CNI_PATH`), run inside the network holder, instead of the native SDN. | `1` → on, and only if a config exists; anything else → native SDN. | Has no effect when the CRI runs as root: root always uses the node's CNI configuration. |
| `DELONIX_TRACE_UNPUBLISH` | `crates/adapters/delonix-sdn/src/infra.rs:trace_unpublish` | Records every port unpublish with the function, port, pid, parent pid, executable and a backtrace. | `1` or `stderr` → stderr; any other value → appended to that file. | A diagnostic tool, zero cost when unset. Kept for the investigation in `AGENTS.md`, *«RESOLVIDO — as portas publicadas morriam sozinhas»*. |

## Build and images

| Variable | Read by | Purpose | Values / default | Notes |
|---|---|---|---|---|
| `DELONIX_INSECURE_REGISTRIES` | `crates/adapters/delonix-oci/src/registry.rs` | Registries that are contacted over plain HTTP instead of HTTPS. | Comma-separated `host` or `host:port` entries (case-insensitive). Unset → HTTPS everywhere except loopback registries, which already use HTTP. | **Traffic and credentials for those hosts travel without TLS.** An explicit opt-in per host, never a range. When an HTTPS connection to a registry fails, the error suggests this variable with the host filled in. |
| `DELONIX_SCAN_ON_PULL` | `bins/delonix-runtime-bin/src/cmd/scan.rs:admission_scan_on_pull` | CVE admission policy applied after every image pull. | Unset/empty → off. `warn` → scan and report. `low`/`medium`/`high`/`critical` → remove the image and refuse when a vulnerability of at least that severity is found. An unknown value refuses the pull. | A fail-closed gate: a typo does not turn it off. An image without an SBOM is admitted with a warning. |
| `DELONIX_ADVISORIES` | `bins/delonix-runtime-bin/src/cmd/scan.rs:load_advisories` | Path to an advisory database file used by `image scan`. | A file path. | Used only when no synced database exists at `<root>/advisories.json`; the synced one wins. Without either, the embedded placeholder is used and the scan says so. |
| `DELONIX_ADVISORY_FEED` | `bins/delonix-runtime-bin/src/cmd/scan.rs:cmd_scan_update` | Source for `delonix image scan --update`. | A URL or file (OSV or native format). | `--feed` wins. Without either, `--update` fails with an error naming both. |

## Cloud Hypervisor and VM resource tuning

The libvirt ceilings below are written into the generated domain XML
(`crates/adapters/delonix-vm/src/lib.rs:libvirt_domain_xml`); they do not change a domain that
already exists until it is booted again.

| Variable | Read by | Purpose | Values / default | Notes |
|---|---|---|---|---|
| `DELONIX_HYPERVISOR_FW` | `crates/adapters/delonix-vm/src/lib.rs:default_ch_firmware` | Firmware Cloud Hypervisor boots when `--firmware` is not given. | A file path, used only if it exists. Otherwise the first existing of `DEFAULT_CH_FIRMWARES` (EDK2 `CLOUDHV.fd` before `hypervisor-fw`). | See [Building microVMs](microvm-setup.md) for why the EDK2 build comes first. |
| `DELONIX_VM_RESERVE_MIB` | `crates/adapters/delonix-vm/src/lib.rs:vm_admission_check` | Memory kept free for the host when admitting a VM: a VM is refused if its memory plus this reserve exceeds `MemAvailable`. | MiB; default `2048`; an unparsable value falls back to 2048. | Lowering it risks the host OOM-killing things. |
| `DELONIX_VM_MEM_HARD_LIMIT` | `crates/adapters/delonix-vm/src/lib.rs:mem_hard_limit_kib` | Whether libvirt domains get a `<memtune><hard_limit>` on the whole QEMU process. | `off` → no hard limit; anything else → on. | |
| `DELONIX_VM_MEM_OVERHEAD_PCT` | `crates/adapters/delonix-vm/src/lib.rs:mem_hard_limit_kib` | Margin above guest memory allowed by the hard limit. | Percent, accepted range 5–200; default `25`; at least 1 GiB of margin. Out of range → 25. | |
| `DELONIX_VM_CPU_QUOTA_CORES` | `crates/adapters/delonix-vm/src/lib.rs:cpu_quota_micros` | CPU ceiling (`<cputune><quota>`) for a libvirt domain, in cores. | Unset → vCPUs + 1 (the extra core is for QEMU's emulator and IO threads). A positive number → that many cores. `off` → no ceiling. | |
| `DELONIX_VM_IO_MAX_BPS` | `crates/adapters/delonix-vm/src/lib.rs:vm_iotune_xml` | Root disk throughput ceiling (`<iotune><total_bytes_sec>`). | Bytes/s, positive integer. Unset or 0 → no ceiling (opt-in). | |
| `DELONIX_VM_IO_MAX_IOPS` | `crates/adapters/delonix-vm/src/lib.rs:vm_iotune_xml` | Root disk IOPS ceiling (`<iotune><total_iops_sec>`). | Positive integer. Unset or 0 → no ceiling (opt-in). | |

### Container resource budget

These tune the defaults and the aggregate ceiling the engine applies to containers
(`crates/adapters/delonix-linux/src/lib.rs`).

| Variable | Read by | Purpose | Values / default | Notes |
|---|---|---|---|---|
| `DELONIX_RESERVE_PCT` | `crates/adapters/delonix-linux/src/lib.rs:host_reserve_pct` | Share of the host (memory and CPU) the engine's cgroup slice may use in total. | Percent, accepted range 10–95; default `85`. Out of range → 85. | Also the base of the per-workload defaults below. |
| `DELONIX_DEFAULT_PCT` | `crates/adapters/delonix-linux/src/lib.rs:default_workload_pct` | Share of the engine's budget one workload may take when it declares no limits. | Percent, accepted range 1–100; default `25`. Out of range → 25. | Memory default is at least 64 MiB; CPU default is clamped between 0.25 and 1.0 cores. |
| `DELONIX_SWAP_MAX` | `crates/adapters/delonix-linux/src/lib.rs:swap_max_value` | `memory.swap.max` of a container's cgroup. | A cgroup value; default `0` (no swap). `max` restores unlimited swap. | Swap turns a memory limit into a soft one. |
| `DELONIX_IO_MAX_BPS` | `crates/adapters/delonix-linux/src/lib.rs:host_io_max_bps` | Aggregate disk read/write ceiling (`io.max`) of the engine slice. | Bytes/s; default `500000000` (500 MB/s). `0` disables it. An unparsable value → default. | A safety ceiling against one container saturating the disk, not fine QoS. Only applies where the `io` controller is available. |

## Providers

### Proxmox VE

Read once at CLI start-up by `bins/delonix-runtime-bin/src/cmd/vmbackends.rs:register_proxmox`.
Nothing is contacted until a VM command selects the `proxmox` backend. Blank values count as unset.
A misconfiguration prints a warning and leaves the backend unregistered; it does not stop other
commands. Background: `docs/adr/0008-proxmox-vm-backend.md`.

| Variable | Read by | Purpose | Values / default | Notes |
|---|---|---|---|---|
| `DELONIX_PROXMOX_URL` | `cmd/vmbackends.rs:register_proxmox_with`; named in the error of `crates/adapters/delonix-vm/src/lib.rs` (`KNOWN_UNREGISTERED`) | API endpoint of the node. Setting it is what enables the backend. | `https://<host>:8006`. | Requires `DELONIX_PROXMOX_NODE` and a credential. |
| `DELONIX_PROXMOX_NODE` | `cmd/vmbackends.rs:register_proxmox_with` | The node name to use (as `GET /nodes` reports it). | e.g. `pve`. No default: the backend never picks a node for you. | |
| `DELONIX_PROXMOX_SECRET` | `cmd/vmbackends.rs:proxmox_auth` | Name of a `kind: Secret` holding the credential. | Secret with `tokenId`+`tokenSecret` (preferred) or `username`+`password`. | Checked **first**. Preferred over the variables below, which end up in shell history and `ps`. |
| `DELONIX_PROXMOX_TOKEN_ID` | `cmd/vmbackends.rs:proxmox_auth` | API token id. | `user@realm!tokenname`. | Used with `DELONIX_PROXMOX_TOKEN`; checked after the secret. |
| `DELONIX_PROXMOX_TOKEN` | `cmd/vmbackends.rs:proxmox_auth` | API token secret. | | |
| `DELONIX_PROXMOX_USER` | `cmd/vmbackends.rs:proxmox_auth` | Account for password authentication. | `root@pam`, … | Used with `DELONIX_PROXMOX_PASSWORD`; checked last. |
| `DELONIX_PROXMOX_PASSWORD` | `cmd/vmbackends.rs:proxmox_auth` | Password for that account. | | |
| `DELONIX_PROXMOX_INSECURE_TLS` | `cmd/vmbackends.rs:register_proxmox_with` | Skips TLS certificate verification for the node. | `1`, `true` or `yes` → skip; default verify. | **Another machine answering in the node's name receives the credential.** Opt-in only, never applied as a fallback after a TLS error. |
| `DELONIX_PROXMOX_BRIDGE` | `cmd/vmbackends.rs:register_proxmox_with` | Default bridge for VM NICs on this node. | A bridge name; the backend's default is `vmbr0`. | A per-VM `bridge:` wins. |
| `DELONIX_PROXMOX_VLAN` | `cmd/vmbackends.rs:parse_vlan` | Default VLAN tag for VM NICs on this node. | 1–4094. Out of range is an **error**, never dropped. | |
| `DELONIX_PROXMOX_CA_FILE` | `cmd/vmbackends.rs:register_proxmox_with` | A CA certificate (PEM) to trust for the node, in addition to the system roots. | Path to a PEM file; unreadable is an **error**. | The way to verify a node whose certificate an internal CA signed, instead of `DELONIX_PROXMOX_INSECURE_TLS`. |
| `DELONIX_PROXMOX_TRACE_ROUTES` | `cmd/vmbackends.rs:register_proxmox_with` (read once, handed to the client as `ClientOptions::trace_routes`; the constant `TRACE_ROUTES_ENV` in `crates/providers/delonix-proxmox/src/lib.rs` names it) and `crates/providers/delonix-proxmox/tests/live.rs:backend` (the live suite, whose run is committed as `docs/proxmox/trace-9.2.2.routes`) | Appends `METHOD /path` of every request to this file — the numerator of the coverage matrix (ADR-0049). | Path to a file; empty = off. | Feed it to `scripts/proxmox_api_inventory.py --trace` to mark routes `supported+tested`. |

### TrueNAS

The TrueNAS provisioner (`kind: Volume` with `spec.provision.truenas`) takes its target from the
manifest and a `kind: Secret`, not from environment variables. The only `DELONIX_TRUENAS_*` names
are test settings — see [Test-only](#test-only).

## Observability

| Variable | Read by | Purpose | Values / default | Notes |
|---|---|---|---|---|
| `DELONIX_OTLP_ENDPOINT` | `crates/adapters/delonix-telemetry/src/telemetry.rs:build_otlp_layer` | Exports tracing spans over OTLP/HTTP (protobuf). | A base URL such as `http://localhost:4318`; `/v1/traces` is appended if missing. Unset or blank → no exporter. | A failure to build the exporter warns and continues with logs only. The service name is `OTEL_SERVICE_NAME` or the executable name. |
| `DELONIX_METRICS_ADDR` | `crates/interfaces/delonix-cri/src/lib.rs` (CRI server start-up) | Enables a Prometheus `/metrics` HTTP listener in `delonix-cri`. | `host:port`, e.g. `127.0.0.1:9100`. Unset → no listener. | A TCP listener: bind it to loopback unless the metrics should be reachable from the network. |
| `DELONIX_WALK_THREADS` | `crates/adapters/delonix-volume/src/lib.rs:walk_threads` (the disk walk behind `system df`, volume usage and the rootless quota) | Sets how many workers the parallel directory walk uses. | A positive integer; `1` → the sequential walk. Unset → the number of CPUs, capped. | The parallel and sequential walks give the SAME total (tested with hardlinks seen by different workers); the variable exists to prove it on a given host, and as the escape hatch if a filesystem misbehaves under concurrent `readdir`. |

## Set by the engine itself / internal

**Do not set these.** The engine writes them on processes it starts; setting them by hand makes a
normal command behave like an internal pass.

| Variable | Read by | Purpose | Values / default | Notes |
|---|---|---|---|---|
| `DELONIX_BIN` | `crates/contexts/delonix-node/src/dispatch.rs:cli_bin` (used by `delonix-cri`, `delonix-mgmt`, `delonix-mcp`) | The `delonix` executable a server runs back for lifecycle operations. | A path. Unset: the `delonix` next to the server's executable, then `delonix` on `PATH`. | Set by `delonix serve …` / `delonix mcp` (`cmd/serve.rs:exec_server`) to the running CLI. Setting it by hand is legitimate only when you start a server binary directly and want it to call a specific CLI. `scripts/cli-tree.sh` and `scripts/docs_cli_gate.py` also read a variable of this name to choose the binary they inspect (see [Clone, build and test](build-and-test.md)). |
| `DELONIX_DISPATCH_VERSION` | `crates/contexts/delonix-node/src/dispatch.rs:check_version` (in `delonix-cri`, `delonix-mgmt`, `delonix-mcp`) | The version the server must be; a server from another release refuses to start. | Set by `cmd/serve.rs:exec_server` to the CLI's version. | A server started directly (for example by a systemd unit) has no expectation and is not checked. |
| `DELONIX_REEXEC_ID` | `bins/delonix-runtime-bin/src/cmd/container.rs` (`cmd_run`, `reexec_env`) | Marks the second pass of `container run/start --net <custom>` or `--pod`, re-executed inside the holder's namespaces, and carries the container id. | Set by `cmd/container.rs:reexec_env`. | Its presence skips checks the first pass already did (port ownership). |
| `DELONIX_REEXEC_IP` | `bins/delonix-runtime-bin/src/cmd/container.rs` | The SDN address the first pass attached, for the second pass to record. | Set by `cmd/container.rs:reexec_env`. | |
| `DELONIX_PIN_SYNC` | `crates/adapters/delonix-sdn/src/pin_userns.rs` (`SYNC_ENV`) | File descriptors of the handshake pipes between the caller and the network pin while user-namespace maps are written. | `<read-fd>,<write-fd>`. | Only set when the pin must create new namespaces; an adopting pin never gets it. |
| `DELONIX_DELEGATE_ATTEMPTED` | `bins/delonix-runtime-bin/src/cmd/kindmode.rs:reexec_under_delegated_scope` | Guards the single automatic re-exec of `cluster create` under a delegated scope, so a host that still lacks `cpu` shows the real error instead of looping. | Set to `1` on the re-executed process. | To disable the re-exec, use `DELONIX_NO_AUTO_DELEGATE`. |
| `DELONIX_INTERNAL` | set on children by `crates/adapters/delonix-sdn/src/infra.rs` (`start_control` and the other holder spawns), `crates/adapters/delonix-vm/src/lib.rs:launch_vmm`, `crates/interfaces/delonix-cri/src/runtime_svc/lifecycle.rs`, `spdy.rs`, `streaming.rs` | Marks a machine-to-machine invocation. | Set to `1`. | **No reader in the current code**: the comment in `runtime_svc/lifecycle.rs:delonix` says it "bypasses the grouped-commands barrier", but nothing reads the variable any more. It is still written, and it appears in the environ of network processes (see the tests of `infra.rs:env_names_this_root`). |

## Build-time

Compile-time values: they are fixed when the binary is built and cannot be changed by setting a
variable when the program runs.

| Variable | Read by | Purpose | Values / default | Notes |
|---|---|---|---|---|
| `DELONIX_GIT_HASH` | written by `bins/delonix-runtime-bin/build.rs`; read with `env!` in `bins/delonix-runtime-bin/src/main.rs` | Short commit hash shown by `delonix --version`. | `git rev-parse --short=9 HEAD`, or `unknown` without git. | |
| `DELONIX_GIT_SINCE` | written by `bins/delonix-runtime-bin/build.rs`; read in `src/main.rs` | Distance from the newest tag, shown as `(+N commits since vX.Y.Z)`. | `<count>\|<tag>`, empty at a tag or without git. | This is how you tell two builds with the same version apart. |
| `DELONIX_BUILD_DATE` | written by `bins/delonix-runtime-bin/build.rs`; read in `src/main.rs` and `src/cmd/man.rs` | Build date in `--version` and in generated man pages. | `date -u +%Y-%m-%d`, or `unknown`. | Man pages use it instead of the clock so two generator runs on one build produce the same output. |
| `DELONIX_GIT_COMMIT` | read with `option_env!` in `bins/delonix-runtime-bin/src/cmd/dockerapi.rs` (`/version` response, `GitCommit`) | Commit reported by the Docker API slice. | Taken from the build environment if set; otherwise `unknown`. | **Nothing in the repository sets it** (not `build.rs`, not the workflows), so released binaries report `unknown`. |
| `DELONIX_BPF_OBJECT` | written by `crates/adapters/delonix-sdn/build.rs`; read with `env!` in `crates/adapters/delonix-sdn/src/bpf.rs` | Path of the compiled eBPF flow-accounting object embedded in the binary. | Set only when `clang` and the libbpf headers are present at build time (together with `cfg(bpf_object)`). | Optional: without it the runtime degrades to nftables counters. |
| `DELONIX_ASSET` | `scripts/install.sh` (binary section) | Shell variable holding the release asset file name chosen for this CPU (`delonix` or its `-v3` variant). | Assigned by the script from the download step. | Not read from your environment; setting it before running the installer has no effect. |

## Test-only

These are read only by tests. Without them the live tests **skip** and print `SKIP: … is not set`
(a live test that passes silently without its target would prove nothing).

| Variable | Read by | Purpose | Values / default | Notes |
|---|---|---|---|---|
| `DELONIX_PROXMOX_TEST_URL` | `crates/providers/delonix-proxmox/tests/live.rs:target` | Proxmox node to run the live backend tests against. | `https://<host>:8006`. | Enables the tests. They create and destroy one VM; run only against a node you own. |
| `DELONIX_PROXMOX_TEST_NODE` | `crates/providers/delonix-proxmox/tests/live.rs:target` | Node name. | Default `pve`. | |
| `DELONIX_PROXMOX_TEST_USER` | `crates/providers/delonix-proxmox/tests/live.rs:target` | Account for password authentication. | e.g. `root@pam`. Required. | TLS verification is disabled in these tests. |
| `DELONIX_PROXMOX_TEST_PASS` | `crates/providers/delonix-proxmox/tests/live.rs:target` | Its password. | Required. | |
| `DELONIX_PROXMOX_TEST_STORAGE` | `crates/providers/delonix-proxmox/tests/live.rs` | Storage for the test VM's disk. | Default `local-lvm`. | |
| `DELONIX_PROXMOX_TEST_BACKUP_STORAGE` | `crates/providers/delonix-proxmox/tests/live.rs` | Storage the backup archive lands on — the node may not accept backup content on the disk storage (a thin-LVM pool cannot). | Default: same as `DELONIX_PROXMOX_TEST_STORAGE`. | |
| `DELONIX_PROXMOX_TEST_AGENT_VMID` | `crates/providers/delonix-proxmox/tests/live.rs:o_ip_vem_do_agente_de_um_convidado_a_serio` | An existing VM, with the QEMU guest agent running, whose IP the test reads. | A VM id. | Skipped when unset, even with the URL set. |
| `DELONIX_TRUENAS_TEST_URL` | `crates/providers/delonix-truenas/tests/live.rs:target` | TrueNAS appliance to run the live provisioner tests against. | `https://<host>`. | Enables the tests. They create and destroy `<pool>/dlxlive-<pid>`. |
| `DELONIX_TRUENAS_TEST_POOL` | `crates/providers/delonix-truenas/tests/live.rs:target` | Pool for the test dataset. | Default `tank`. | |
| `DELONIX_TRUENAS_TEST_KEY` | `crates/providers/delonix-truenas/tests/live.rs:target` | API key. | | Used instead of user and password when set. |
| `DELONIX_TRUENAS_TEST_USER` | `crates/providers/delonix-truenas/tests/live.rs:target` | Account for password authentication. | Required without a key. | TLS verification is disabled in these tests. |
| `DELONIX_TRUENAS_TEST_PASS` | `crates/providers/delonix-truenas/tests/live.rs:target` | Its password. | Required without a key. | |
| `DELONIX_UPDATE_FIXTURES` | `crates/adapters/delonix-linux/tests/advisor_fixtures.rs:goldens_match_the_rules_as_they_are_today` | Rewrites the advisor golden fixtures in `crates/adapters/delonix-linux/tests/fixtures/advisor/` instead of comparing against them. | Set (any value) → rewrite. | Regenerate in the **same commit** as the rule change. |

Run the live tests (replace the placeholders; never commit real credentials):

```bash
DELONIX_PROXMOX_TEST_URL=https://<node>:8006 \
DELONIX_PROXMOX_TEST_NODE=pve \
DELONIX_PROXMOX_TEST_USER=root@pam \
DELONIX_PROXMOX_TEST_PASS='<password>' \
  cargo test -p delonix-proxmox --test live -- --nocapture

DELONIX_TRUENAS_TEST_URL=https://<appliance> \
DELONIX_TRUENAS_TEST_USER=<user> \
DELONIX_TRUENAS_TEST_PASS='<password>' \
DELONIX_TRUENAS_TEST_POOL=tank \
  cargo test -p delonix-truenas --test live -- --nocapture

DELONIX_UPDATE_FIXTURES=1 cargo test -p delonix-linux --test advisor_fixtures
```

## Names that look like variables but are not

A few `DELONIX_*` names appear in the code without being read from any process environment.
They are excluded from the check above by `NOT_ENV` in `scripts/dev_docs.py`, each with its reason,
and you never need to set them:

- `DELONIX_CRI_SOCKET` — a Rust constant in `bins/delonix-runtime-bin/src/cmd/cluster.rs`, passed to
  `kubeadm … --cri-socket=`. The socket the CRI server listens on is `DELONIX_CRI_ADDR`.
- `DELONIX_ROOTX`, `DELONIX_ROOT_BACKUP` — test fixtures in `crates/adapters/delonix-sdn/src/infra.rs`,
  proving that reading a process environ matches whole names, not prefixes.
- `DELONIX_IMAGE`, `DELONIX_DISTRO`, `DELONIX_RELEASE`, `DELONIX_BUILT_BY`, `DELONIX_BASE_IMAGE`,
  `DELONIX_BASE_SHA256`, `DELONIX_K8S_VERSION`, `DELONIX_OFFLINE`, `DELONIX_NODE_EXPORTER`,
  `DELONIX_EXTRA_PACKAGES` — keys of the provenance file `/etc/delonix-image-release` that
  `image vm build` writes **inside** a built VM image (`bins/delonix-runtime-bin/src/cmd/vmimage.rs`).
  Read them in the guest with `cat /etc/delonix-image-release`; no process reads them.

---

**Next:** [Glossary](glossary.md) — the terms you meet in this repository, with their Delonix meaning and where each is explained.
