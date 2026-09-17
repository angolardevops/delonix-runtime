# 4. Cloud native primer

The engine is a thin, careful layer over Linux kernel features and a handful of open
specifications. This page gives you just enough of each concept to read the code, and says
**where it lives** in this repository. For depth, follow the official links — they are better
than any summary here.

Each section has three parts: the concept, **In Delonix** (files and symbols you can `grep`),
and **Read more**. Paths are relative to the repository root.

For orientation in the wider ecosystem, the [CNCF Landscape](https://landscape.cncf.io/) and the
[CNCF Glossary](https://glossary.cncf.io/) are good maps. Comparisons with runc, crun, containerd
or Podman appear only where they help explain a design choice.

---

## 4.1 Linux namespaces and rootless operation

A **namespace** gives a process its own view of one kind of global resource. A container is,
at its core, a process started in a fresh set of them: **mount** (its own filesystem tree),
**PID** (its own process numbering, with itself as PID 1), **network** (its own interfaces and
routes), **IPC**, **UTS** (hostname), **cgroup** (its own view of the cgroup tree) and **user**.

The **user namespace** is what makes rootless containers possible. Inside it, a process can be
uid 0 with full capabilities *over resources owned by that namespace*, while on the host it is an
ordinary user. The mapping between inside and outside uids is written to
`/proc/<pid>/uid_map` and `gid_map`. An unprivileged user can map only its own uid; mapping a
*range* requires the setuid helpers `newuidmap`/`newgidmap`, which check `/etc/subuid` and
`/etc/subgid`. Some distributions additionally restrict unprivileged user namespaces through
AppArmor — see [1. Environment](01-environment.md) for the practical consequences.

**In Delonix**

- `fn spawn` in `crates/adapters/delonix-linux/src/lib.rs` builds the `CloneFlags`
  (`CLONE_NEWNS`, `CLONE_NEWPID`, `CLONE_NEWNET`, `CLONE_NEWUSER`, …) and calls `nix::sched::clone`.
  IPC/UTS sharing between pod members is handled by `setns` in `container_init`.
- `write_userns_maps` in the same file writes the maps from the parent: a single-uid map
  (`0 <euid> 1`) rootless, or a subuid range through `newuidmap`/`newgidmap` when
  `have_subid_helpers()` says they are available. `USERNS_UID_BASE`/`USERNS_RANGE` define the range
  used when running as root.
- `setup_rootfs` mounts the container's root and calls `pivot_root`; `container_init` is the
  code that runs inside the new namespaces before `execvp`.
- Rootless operations on files owned by mapped subuids re-execute the binary inside a mapped user
  namespace: `reexec_mapped`, `reexec_mapped_hold`, `remove_tree_mapped`.

**Read more:** [`namespaces(7)`](https://man7.org/linux/man-pages/man7/namespaces.7.html),
[`user_namespaces(7)`](https://man7.org/linux/man-pages/man7/user_namespaces.7.html),
[`newuidmap(1)`](https://man7.org/linux/man-pages/man1/newuidmap.1.html),
[`subuid(5)`](https://man7.org/linux/man-pages/man5/subuid.5.html),
[`pivot_root(2)`](https://man7.org/linux/man-pages/man2/pivot_root.2.html).

---

## 4.2 cgroups v2 and delegation

**Control groups** limit and account for resources (memory, CPU, PIDs, I/O) for a set of
processes. cgroup v2 is a single tree mounted at `/sys/fs/cgroup`; a directory is a group, and
files like `memory.max`, `cpu.max`, `pids.max` and `memory.events` are its interface. A
controller is available to a child only if the parent lists it in `cgroup.subtree_control`.

An unprivileged user can manage a subtree only if it has been **delegated** to them. On systemd
hosts, `user@<uid>.service` typically delegates some controllers, and
`systemd-run --user --scope -p Delegate=yes` creates a delegated scope on demand. A process in an
SSH session scope usually sits *outside* the delegated subtree, and the "no internal processes"
rule prevents moving it in — so limits can silently not apply there. The engine is v2-only.

**In Delonix**

- Root mode places containers under `delonix_runtime_core::DELONIX_SLICE`
  (`/sys/fs/cgroup/delonix.slice`).
- Rootless mode finds the user's service cgroup and creates leaves under
  `<user@uid.service>/dlx-containers` — see `user_service_base` and `try_delegated_base` in
  `crates/adapters/delonix-linux/src/lib.rs`. `cgroup_limits_apply` answers "will limits apply
  on this host?" without starting a container.
- The design decision about the intermediate level is
  [ADR-0015](../adr/0015-intermediate-cgroup-level.md); how the CRI follows the kubelet's cgroup
  hierarchy is [ADR-0038](../adr/0038-cri-follows-kubelet-resource-model.md), with the kubelet's
  parent validated by `KubeCgroupParent::parse` in
  `crates/foundation/delonix-runtime-core/src/lib.rs`.

**Read more:** [Linux kernel — Control Group v2](https://docs.kernel.org/admin-guide/cgroup-v2.html),
[systemd — Control Group APIs and Delegation](https://systemd.io/CGROUP_DELEGATION/),
[`cgroups(7)`](https://man7.org/linux/man-pages/man7/cgroups.7.html).

---

## 4.3 Capabilities, seccomp, AppArmor, masked paths

Root's power is split into **capabilities** (`CAP_NET_ADMIN`, `CAP_SYS_ADMIN`, …). A container
keeps a small default set and drops the rest. **seccomp** installs a BPF filter that allows or
denies system calls, reducing kernel attack surface. **AppArmor** (and SELinux on other
distributions) are Linux Security Modules that confine a process by profile. Finally, runtimes
**mask** sensitive `/proc` and `/sys` paths (bind something empty over them) and make others
read-only, because those files leak host information or allow host control.

A subtle point the code documents: `clone3` passes its flags through a pointer that a seccomp
filter cannot inspect, so a filter that blocks `clone(CLONE_NEWUSER)` must also make `clone3`
fail with `ENOSYS` to force libc back to the filterable `clone`.

**In Delonix**

- Capabilities: `KEPT_CAPS` and `resolve_cap_keep` in
  `crates/adapters/delonix-linux/src/capabilities.rs`; `drop_capabilities` in `lib.rs`.
- seccomp: `apply_seccomp` in `crates/adapters/delonix-linux/src/lib.rs` (built with the
  [`seccompiler`](https://docs.rs/seccompiler) crate, including the `clone3` → `ENOSYS`
  pre-filter); custom JSON profiles are parsed and compiled in `seccomp_profile.rs` (`parse`,
  `compile`).
- AppArmor: `apply_apparmor` in `lib.rs`.
- Masked and read-only paths: `DEFAULT_MASKED_PATHS`, `DEFAULT_READONLY_PATHS`,
  `apply_masked_paths`, `apply_readonly_paths`, `mask_proc_paths` in `lib.rs`.
- The node-level security decisions (policy, admission, events, score) are a separate pure crate:
  `evaluate` in `crates/contexts/delonix-security-runtime/src/admission.rs`
  ([ADR-0026](../adr/0026-security-runtime-decision-crate.md)).

**Read more:** [`capabilities(7)`](https://man7.org/linux/man-pages/man7/capabilities.7.html),
[kernel — Seccomp BPF](https://docs.kernel.org/userspace-api/seccomp_filter.html),
[AppArmor documentation](https://gitlab.com/apparmor/apparmor/-/wikis/Documentation),
[OCI runtime spec — Linux config](https://github.com/opencontainers/runtime-spec/blob/main/config-linux.md)
(the `maskedPaths`/`readonlyPaths`/`seccomp` fields other runtimes consume).

---

## 4.4 OCI images, content-addressed storage and overlayfs

The **Open Container Initiative** publishes three specifications:

- the **image spec** — an image is a *manifest* (JSON) that points to a *config* and an ordered
  list of *layers* (tarballs), and possibly an *index* that points to one manifest per platform;
- the **distribution spec** — the HTTP API registries serve (`/v2/<name>/manifests/<ref>`,
  `/v2/<name>/blobs/<digest>`, token auth);
- the **runtime spec** — how a runtime such as runc or crun is told to run a filesystem bundle.

Everything is **content-addressed**: a blob is named by the SHA-256 digest of its bytes, so a
client verifies what it downloaded by hashing it. Pulling by digest (`name@sha256:…`) is only a
guarantee if the *manifest* is checked against that digest as well as each blob against the
manifest.

At run time, layers are stacked with **overlayfs**: read-only `lowerdir`s, a writable `upperdir`
where changes are copied up, and a `workdir`. Many containers can share the same lower layers.

**In Delonix**

- Registry client (distribution spec): `crates/adapters/delonix-oci/src/registry.rs` —
  `pull_from_registry*` functions, the `ACCEPT_MANIFEST` media types, and
  `verify_manifest_digest`. Types come from the [`oci-spec`](https://docs.rs/oci-spec) crate.
- Content-addressed blob store: `Cas` in `crates/adapters/delonix-oci/src/cas.rs`.
- Writing an OCI image layout archive: `write_oci_archive` in `save.rs`.
- Overlay preparation: `ImageStore::prepare_overlay` in `overlay.rs` writes an `overlay-lowers`
  marker (`LOWERS_FILE`); the mount itself happens inside the container's user and mount
  namespaces in `mount_overlay_if_marked` (`crates/adapters/delonix-linux/src/lib.rs`), using
  the new mount API — see [ADR-0016](../adr/0016-filesystem-under-the-state-root.md) and
  [ADR-0037](../adr/0037-overlay-mount-new-api.md).
- The engine runs containers itself rather than handing an OCI runtime bundle to runc/crun.

**Read more:** [OCI image spec](https://github.com/opencontainers/image-spec),
[OCI distribution spec](https://github.com/opencontainers/distribution-spec),
[OCI runtime spec](https://github.com/opencontainers/runtime-spec),
[kernel — Overlay Filesystem](https://docs.kernel.org/filesystems/overlayfs.html).

---

## 4.5 Container networking

Linux networking building blocks:

- a **network namespace** has its own interfaces, routes and firewall;
- a **veth pair** is a virtual cable with one end in each namespace;
- a **bridge** is a virtual switch joining many veth ends;
- **nftables** is the kernel packet filter and NAT engine; **DNAT** rewrites a destination (how a
  published port reaches a container), and **conntrack** tracks flows so reply traffic of an
  allowed connection passes (`ct state established,related`);
- **slirp4netns** gives an unprivileged network namespace outbound connectivity by emulating a
  TCP/IP stack in user space, and forwards host ports into it;
- **VXLAN** carries L2 frames over UDP between hosts, and **WireGuard** encrypts a tunnel.

**CNI** (Container Network Interface) is a spec where a runtime executes plugin binaries
(`bridge`, `host-local`, `portmap`, …) with `ADD`/`DEL` commands and a JSON config from
`/etc/cni/net.d`. Kubernetes runtimes use it for pod networking.

**In Delonix**

- Rootless networking cannot create interfaces on the host, so the engine keeps a long-lived
  **holder** network namespace: a minimal *pin* process owns the namespaces, and a restartable
  *control* process serves a Unix socket. See `start_pin`, `start_control` and `ensure_up` in
  `crates/adapters/delonix-sdn/src/infra.rs`.
- Attaching a workload: `attach_container` (IPAM + control command) and `do_attach` (veth into the
  bridge, inside the holder). Bridge names come from `bridge_name` in the dependency-free
  `crates/foundation/delonix-net-rules/src/lib.rs`.
- Outbound and port forwarding: `slirp_attach` and `slirp_add_hostfwd` in
  `crates/adapters/delonix-sdn/src/lib.rs` (which spawn `slirp4netns`); in-holder publishing in
  `publish_port`/`do_publish` (`infra.rs`).
- Firewall: `table ip dlxing` with base chains `fwguard`, `fwdeny`, `fwcont` and the `fwmap`
  verdict map (`FWMAP`), generated in `infra.rs` (`do_firewall`, `apply_firewall_all`,
  `ns_set_join` for namespace isolation sets).
- Internal DNS (`<name>.<namespace>.delonix.internal`): `dns_server_main`, `handle_dns`,
  `dns_resolve_for`, `dns_resolve_multi_for` in `infra.rs`.
- Overlay networks: `set_vxlan` (`infra.rs`) and the WireGuard helpers in
  `crates/adapters/delonix-sdn/src/wg.rs`, orchestrated by `realize_overlay` in
  `bins/delonix-runtime-bin/src/cmd/network.rs`.
- CNI: `crates/adapters/delonix-sdn/src/cni.rs` — `add`, `del`, `readiness`,
  `attach_named_netns`. Rootless use is opt-in (`enabled_conf` checks `DELONIX_CNI=1`); the root
  CRI path uses the node's CNI chain (`root_cni_readiness` in
  `crates/interfaces/delonix-cri/src/runtime_svc.rs`).
- Topology decisions: [ADR-0013](../adr/0013-network-topology.md),
  [ADR-0014](../adr/0014-runtime-dir-per-root.md).

**Read more:** [`network_namespaces(7)`](https://man7.org/linux/man-pages/man7/network_namespaces.7.html),
[`veth(4)`](https://man7.org/linux/man-pages/man4/veth.4.html),
[nftables wiki](https://wiki.nftables.org/),
[slirp4netns](https://github.com/rootless-containers/slirp4netns),
[kernel — VXLAN](https://docs.kernel.org/networking/vxlan.html),
[WireGuard](https://www.wireguard.com/),
[CNI](https://www.cni.dev/) and its [specification](https://www.cni.dev/docs/spec/).

---

## 4.6 Kubernetes: CRI, kubelet, kubeadm and kind

The **kubelet** is the Kubernetes node agent. It does not run containers itself; it talks to a
container runtime over the **Container Runtime Interface**, a gRPC API (`RuntimeService`,
`ImageService`) served on a local Unix socket. The kubelet creates a *pod sandbox*
(`RunPodSandbox`) and then containers inside it. It also has a **cgroup driver** setting
(`systemd` or `cgroupfs`) that must match how the runtime manages cgroups, or pod cgroups and
container cgroups diverge.

**kubeadm** bootstraps a cluster on existing machines (`kubeadm init`, `kubeadm join`).
**kind** runs Kubernetes nodes as containers built from the `kindest/node` image.

**In Delonix**

- `crates/interfaces/delonix-cri` is a CRI `runtime.v1` server. The protobuf is
  `proto/api.proto` inside that crate, compiled by `build.rs` with `tonic-build`.
  The binary is `src/bin/delonix-cri.rs`.
- The cgroup driver reported to the kubelet: `engine_cgroup_driver` in `runtime_svc.rs`; its doc
  comment records why the answer is what it is and what would have to change for the other one.
- A round-trip over real gRPC is tested in `crates/interfaces/delonix-cri/tests/grpc_status.rs`.
- Cluster bootstrap commands: kubeadm over SSH in `bins/delonix-runtime-bin/src/cmd/cluster.rs`
  (with `kubeadm_config.rs`, `etcd.rs`, `lb.rs`), and kind-style local clusters in `kindmode.rs`.

**Read more:** [Kubernetes — Container Runtime Interface](https://kubernetes.io/docs/concepts/architecture/cri/),
[cri-api repository](https://github.com/kubernetes/cri-api),
[Configuring a cgroup driver](https://kubernetes.io/docs/tasks/administer-cluster/kubeadm/configure-cgroup-driver/),
[kubeadm](https://kubernetes.io/docs/reference/setup-tools/kubeadm/),
[kind](https://kind.sigs.k8s.io/).

---

## 4.7 Virtualization: KVM, virtio, Cloud Hypervisor, libvirt, cloud-init

**KVM** is the kernel's hypervisor, exposed as `/dev/kvm`; a user-space **VMM** (QEMU, Cloud
Hypervisor) uses it to run guests. **virtio** is the family of paravirtual devices (disk, network,
9p filesystem sharing) guests use for efficient I/O. **Cloud Hypervisor** is a Rust VMM focused on
cloud workloads that can run unprivileged with access to `/dev/kvm`; it boots guests through a
firmware (an EDK2 UEFI build or `rust-hypervisor-firmware`) or directly from a kernel image.
**libvirt** manages QEMU/KVM domains described in XML, via `virsh` and `libvirtd`.

Cloud images are generic; per-instance configuration (hostname, SSH keys, users, network) comes
from **cloud-init**, which reads a datasource. The **NoCloud** datasource is a small ISO labelled
`cidata` holding `user-data`, `meta-data` and optionally `network-config`.

**In Delonix**

- The port is `VmBackend` in `crates/adapters/delonix-vm/src/lib.rs`, implemented by
  `CloudHypervisorBackend` and `LibvirtBackend` there and by `ProxmoxBackend` in
  `crates/providers/delonix-proxmox` ([ADR-0008](../adr/0008-proxmox-vm-backend.md)).
- Cloud Hypervisor firmware search order: `DEFAULT_CH_FIRMWARES` (EDK2 `CLOUDHV.fd` before
  `hypervisor-fw`); the VMM command line is built in `boot_ch`.
- libvirt domain XML: `libvirt_domain_xml`.
- NoCloud seed generation: `generate_seed_iso` in `bins/delonix-runtime-bin/src/cmd/vm.rs`.
- VMs on the rootless SDN get a DHCP lease derived from their MAC: `dhcp_lease_ip` in
  `crates/adapters/delonix-sdn/src/infra.rs`.
- Practical setup and known host pitfalls: [9. MicroVM setup](09-microvm-setup.md).

**Read more:** [kernel — KVM](https://docs.kernel.org/virt/kvm/index.html),
[virtio specification (OASIS)](https://docs.oasis-open.org/virtio/virtio/),
[Cloud Hypervisor](https://www.cloudhypervisor.org/) and its
[documentation](https://github.com/cloud-hypervisor/cloud-hypervisor/tree/main/docs),
[libvirt](https://libvirt.org/docs.html),
[cloud-init NoCloud](https://docs.cloud-init.io/en/latest/reference/datasources/nocloud.html).

---

## 4.8 Declarative reconciliation

Kubernetes popularised a model where users submit **desired state** as typed objects
(`apiVersion`, `kind`, `metadata`, `spec`), and controllers repeatedly compare it with **actual
state** and act to converge. `kubectl apply` adds a **three-way diff**: it stores the last
applied configuration on the object, so it can tell "you removed this field from your file"
(revert it) from "someone set this field by hand" (leave it alone).

**In Delonix**

- The engine has its own Kinds in API groups (`delonix api-resources` lists them). Facts about
  each Kind (domain, whether it converges, teardown, namespacing) live in one table: `KindFacts`
  in `crates/contexts/delonix-stack/src/kinds.rs`.
- The planner is pure: `plan(desired, actual, stack)` in
  `crates/contexts/delonix-stack/src/reconcile.rs`. The module doc has the three-way truth table,
  and the last-applied spec is stored on the resource itself under the `LAST_APPLIED`
  annotation (`delonix.io/last-applied`) — there is no separate state file.
- Ownership is a label on the resource; revision history is in `revision.rs`
  ([ADR-0019](../adr/0019-stack-revision-history.md)).
- Manifests are parsed in `bins/delonix-runtime-bin/src/cmd/manifest.rs`; `stack plan`/`apply`
  live in `cmd/stack.rs`.
- There is no controller loop running in the background: reconciliation happens when a command
  runs (daemonless). The proposed pull reconciler keeps that property by being a systemd timer
  that invokes the same apply, not a resident process
  ([ADR-0021](../adr/0021-gitops-pull-reconciler.md), status *Proposed*).

**Read more:** [Kubernetes — Objects](https://kubernetes.io/docs/concepts/overview/working-with-objects/),
[Controllers](https://kubernetes.io/docs/concepts/architecture/controller/),
[Declarative management with `kubectl apply`](https://kubernetes.io/docs/tasks/manage-kubernetes-objects/declarative-config/).

---

## 4.9 Observability and the MCP interface

**OpenTelemetry** is a CNCF standard for traces, metrics and logs, exported over **OTLP** to a
collector. **Prometheus** scrapes metrics from an HTTP `/metrics` endpoint in a text exposition
format. The **Model Context Protocol** is an open protocol that lets AI clients discover and call
tools exposed by a server, commonly over stdio.

**In Delonix**

- Structured logging and optional OTLP spans: `init` in
  `crates/adapters/delonix-telemetry/src/telemetry.rs` (spans are exported only when
  `DELONIX_OTLP_ENDPOINT` is set; the exporter runs on its own thread so the synchronous CLI needs
  no async runtime).
- Prometheus registry (prefix `delonix`) and text encoding: `crates/adapters/delonix-telemetry/src/metrics.rs`
  (`encode`). The local management API serves `/metrics` in `crates/interfaces/delonix-mgmt/src/lib.rs`.
- MCP: `crates/interfaces/delonix-mcp` (built on [`rmcp`](https://docs.rs/rmcp), stdio transport),
  with the binary in `bins/delonix-mcp-bin`. Scope and limits:
  [ADR-0025](../adr/0025-mcp-local-ai-control-surface.md).

**Read more:** [OpenTelemetry documentation](https://opentelemetry.io/docs/),
[OTLP specification](https://opentelemetry.io/docs/specs/otlp/),
[Prometheus — exposition formats](https://prometheus.io/docs/instrumenting/exposition_formats/),
[Model Context Protocol](https://modelcontextprotocol.io/).

---

## 4.10 Daemonless, in one paragraph

containerd and the Docker Engine keep a resident daemon that owns container state; Podman showed
that a runtime can instead be a command that exits, with per-container helper processes and
systemd for anything that must persist. Delonix follows the second model: the CLI does the work
and exits, state is files under the state root guarded by `flock` (see
[3. Rust primer §3.8](03-rust-primer.md#38-concurrency-and-shared-state)), a supervisor process
exists per detached container, the network holder exists only while something needs it, and boot
persistence is systemd units (`bins/delonix-runtime-bin/src/cmd/boot.rs`). The consequences —
good and bad — are discussed in [5. Architecture](05-architecture.md) and
[7. System design interview](07-system-design-interview.md).
