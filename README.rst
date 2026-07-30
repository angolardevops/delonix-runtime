==============
Delonix Engine
==============

A **daemonless, rootless-first, kernel-native** container and microVM **engine**,
written in Rust — *the open-source engine at the foundation of the Delonix
platform.* It creates and manages Linux containers directly through namespaces,
cgroups v2 and nftables — no supervising daemon, not even a monitor per container
— plus declarative microVMs on Cloud Hypervisor or libvirt/KVM. It is homologous
to Docker/Podman, with **rootless operation as a first-class design goal** rather
than an afterthought, and it ships its own Kubernetes CRI so a ``kubelet`` can
talk to it with no containerd in between.

Not a low-level OCI *runtime* (that's ``runc``/``crun``): Delonix is a full
container **and** VM engine — build, run, network, firewall, store, and
bootstrap Kubernetes clusters, from one binary.

:Version: 0.38.0
:License: Apache-2.0
:Docs: https://angolardevops.github.io/delonix-runtime/
:Repo: https://github.com/angolardevops/delonix-runtime

Why it's different
==================

.. list-table::
   :header-rows: 1
   :widths: 22 26 26 26

   * -
     - Docker
     - Podman
     - Delonix
   * - Daemon
     - ``dockerd`` (root)
     - none (a ``conmon`` per container)
     - none — and no residing per-container monitor either
   * - Rootless
     - opt-in
     - yes (a slirp/pasta per container)
     - default — one shared ``slirp4netns`` + an nftables ingress
   * - VMs
     - —
     - ``machine`` (for itself)
     - first-class declarative microVMs (Cloud Hypervisor / libvirt)
   * - Kubernetes
     - —
     - —
     - own CRI + ``kubeadm`` bootstrap from scratch (``delonix cluster``)
   * - Firewall
     - basic
     - basic
     - per-container L4 (``ingress``/``egress``) + declarative ``kind: FirewallPolicy``
   * - Observability
     - ``stats``
     - ``stats``
     - eBPF per-container flow accounting (``delonix net flow``)

Highlights
==========

- **No daemon.** Every command is an ephemeral process that speaks straight to
  the kernel (``clone()``, namespaces, cgroups v2, ``pivot_root``) and exits.
  State lives as JSON under ``$DELONIX_ROOT``; opportunistic reapers sweep
  orphans (a slirp with no target, a hostfwd with no container).
- **Rootless SDN.** A single holder network namespace + ``delonix0`` bridge +
  one shared ``slirp4netns``, with nftables DNAT for port publishing. Because a
  published port is *dataplane state, not container state*, ports and volumes can
  be swapped **hot**, without restarting the container.
- **Real pods.** ``kind: Pod`` and ``delonix pod`` run N containers as one unit,
  sharing the pod's network namespace (same IP, ``localhost`` between them), IPC
  and UTS — a Kubernetes-style pod, rootless (PID sharing is a follow-up).
- **Declarative microVMs.** ``kind: Vm`` on a pluggable ``VmBackend`` (Cloud
  Hypervisor or libvirt), with per-instance cloud-init and libvirt system
  checkpoints (``vm snapshot``/``restore``).
- **One workload model.** ``kind: Workload`` (``spec.type: container | vm |
  pod | microvm``) is a single declarative object that lowers to the right Kind,
  and ``delonix workload ls/describe/stop/rm`` manages containers **and** VMs
  from one imperative surface.
- **Structured output.** ``-o json`` on every list command emits stable,
  language-independent field names — ``delonix workload ls -o json | jq`` is the
  automation path.
- **Network storage.** ``kind: Storage`` mounts NFS/CIFS/WebDAV shares from a NAS
  (TrueNAS/Synology/Samba/Nextcloud) as named volumes, k8s-PersistentVolume style.
- **Firewall as code.** A unified ``ingress``/``egress`` command surface and
  declarative ``kind: FirewallPolicy`` manifests (k8s NetworkPolicy style,
  ``direction: ingress``/``egress``) that compile to nftables — plus a
  separate ``kind: Ingress`` for k8s-style L7 HTTP routing (host/path →
  backend service), not to be confused with the L4 firewall.
- **eBPF observability.** ``delonix net flow`` attaches tc/clsact classifiers to the
  SDN veths for live per-container traffic — activating only when it has the
  capability, degrading silently to veth counters otherwise.
- **Kubernetes, end to end.** A CRI server (``delonix-cri``) and
  ``delonix cluster kubeadm`` provision VMs and bootstrap a real cluster whose
  node runtime *is* Delonix.

Install
=======

One command — installs the binary **and** everything the runtime needs on the
host (slirp4netns, uidmap/subuid ranges, nftables, VM backend), so a fresh
machine is fully functional with no manual steps. Works on Debian/Ubuntu,
Fedora/RHEL, openSUSE and Arch families (uses ``sudo`` for packages):

.. code-block:: bash

   curl -fsSL https://github.com/angolardevops/delonix-runtime/releases/latest/download/install.sh | bash

The installer detects your hardware (CPU features, RAM, disk, GPU) and picks
the best binary for it — an ``x86-64-v3`` (AVX2) build on modern CPUs, the
generic ``x86-64`` everywhere else — and applies the kernel tuning that
containers, Kubernetes and VMs need (inotify limits, ip_forward,
``br_netfilter``, ``overlay``/``tun`` modules, ...).

Useful flags (pass after ``bash -s --``): ``--no-vm`` skips the microVM stack,
``--no-tune`` skips kernel tuning, ``--with-cri`` also installs ``delonix-cri``
(Kubernetes node), ``--low-ports`` allows publishing ports below 1024,
``--user`` installs to ``~/.local/bin``, ``--version vX.Y.Z`` pins a release.

Publishing port 80 or 443 (``-p 80:80``) needs ``--low-ports``:

.. code-block:: bash

   curl -fsSL .../install.sh | bash -s -- --low-ports

Without it, ``-p 80:80`` is refused — the host-side bind is done by
``slirp4netns`` as your unprivileged user, and the kernel reserves ports below
``net.ipv4.ip_unprivileged_port_start`` (1024 by default). Rootless Podman and
Docker hit the same wall. The flag is **opt-in** because it lowers that
threshold to 80 for the *whole host*: from then on any local program can bind
80-1023. On a workstation that is a fair trade; on a shared or production
machine, the alternative that lowers nothing is a root-owned proxy on port 80
(nginx/haproxy/systemd socket activation) forwarding to a high port. It writes
``/etc/sysctl.d/99-delonix-lowports.conf`` — delete it to revert.

Manual alternative (binary only — you install the runtime deps yourself):

.. code-block:: bash

   curl -fL -o ~/.local/bin/delonix \
     https://github.com/angolardevops/delonix-runtime/releases/latest/download/delonix-x86_64-linux
   chmod +x ~/.local/bin/delonix
   echo 'source <(delonix completion bash)' >> ~/.bashrc

Quickstart
==========

.. code-block:: bash

   # a web service on host port 8080 — no root, no daemon
   delonix container run -d --name web -p 8080:80 nginx
   curl localhost:8080

   delonix container stats            # CPU / memory / PIDs (cgroup v2)
   delonix container logs -f web      # follow logs
   delonix container update web --publish-add 9090:80   # hot re-publish
   delonix container stop web         # the port closes by itself
   delonix container start web        # restart with the same state

Command groups
==============

The CLI is grouped semantically; every group has ``--help`` and most accept a
per-Kind manifest via ``apply -f``. Every list command also takes ``-o json``
for stable, language-independent output (ADR-0005). Full, always-current
reference (embeds the real ``--help``) at
https://angolardevops.github.io/delonix-runtime/cheatsheet.html.

.. list-table::
   :header-rows: 1
   :widths: 16 84

   * - Group
     - What it does
   * - ``container``
     - Lifecycle: run, ps, start, stop, rm, exec, logs, inspect, stats, update, apply.
   * - ``pod``
     - Real multi-container pods (``kind: Pod``): create, ls, describe, rm, logs — N containers sharing netns/IPC/UTS as one unit.
   * - ``image``
     - OCI images: pull, ls, rm, export, scan; with ``--vm``, golden VM images (build/push).
   * - ``build``
     - Build an image from a Dockerfile or Delonixfile (no daemon, no BuildKit).
   * - ``vm``
     - Declarative microVMs: create, ls, status, start, stop, rm, apply, snapshot/restore (libvirt system checkpoints).
   * - ``workload``
     - Unified compute layer over containers **and** VMs (ADR-0002): ls, describe, stop, rm — creation stays declarative via ``kind: Workload``.
   * - ``volumes``
     - Named volumes and bind mounts: create, ls, inspect, snapshot, rm.
   * - ``network``
     - User bridge networks: create, ls, inspect, rm.
   * - ``secret``
     - Encrypted-at-rest secret vault — the producer of ``run --secret``.
   * - ``storage``
     - Network volumes (NFS/CIFS/WebDAV), k8s-PersistentVolume style.
   * - ``sharevolume``
     - An isolated, individually-quota'd slice of a ``Storage`` — several container/vm/pod share one NAS export without seeing each other's data.
   * - ``stack``
     - Apply a whole manifest — every Kind, in dependency order.
   * - ``compose``
     - Native ``docker-compose.yml`` support (up/down/ps/logs/config) — no Docker involved.
   * - ``cluster``
     - Kubernetes from scratch: ``kubeadm`` bootstrap over SSH, full VM provisioning (with automatic HA/HAProxy for multi-control-plane), or manifest generation from a running container/pod (``cluster kube generate``).
   * - ``net``
     - Low-level network/infra, grouped: ``netns`` (rootless ingress infra), ``flow`` (live per-container traffic via eBPF), ``ingress``/``egress`` (L4 firewall), ``httproute`` (embedded L7/HTTP(S) reverse-proxy with hot reload and ``run --expose`` auto-registration), ``tunnel`` (expose a port publicly via pinggy/ngrok/cloudflare), ``boot`` (systemd persistence across reboots).
   * - ``serve``
     - Serve a protocol endpoint on a unix socket, grouped: ``cri`` (Kubernetes ``runtime.v1``), ``api`` (management API, HTTP+JSON), ``docker-api`` (a slice of the Docker Engine API, full container lifecycle).
   * - ``system``
     - The engine itself: events, info, df, prune (GC), monitor, thermal.
   * - ``dash``
     - Interactive htop-style TUI dashboard — RAM/network/disk KPIs, per-container uptime, ``--json`` for scripts/Grafana, plus Prometheus ``/metrics`` on ``serve api``/``serve cri``.
   * - ``completion``
     - Dynamic autocompletion for bash/zsh/fish/elvish/powershell.

Languages
=========

The CLI speaks **English by default**. ``--l18n=pt`` (or ``DELONIX_L18N=pt``)
switches everything — including ``--help`` — to Portuguese, served from a
standard gettext catalog embedded in the binary
(`data/pt.po <crates/delonix-runtime-bin/data/pt.po>`_). Adding a language is
adding a ``.po`` file; no code changes. Containers started without ``--name``
get readable names drawn from Angolan kings, queens and places
(``njinga-benguela-07``) — the project's naming identity.

Manifests
=========

The declarative face, Kubernetes-style: a multi-document YAML
(``apiVersion: delonix.io/v1``) with Kinds — ``Network``, ``Volume``,
``Storage``, ``Image``, ``Vm``, ``Container``, ``Pod``, ``Workload``,
``FirewallPolicy``, ``Ingress`` (k8s-style L7 HTTP routing), ``Egress`` —
applied in dependency
order by ``delonix stack apply``. Ensure-present semantics (idempotent by
name), not a reconciler. ``kind: Pod`` is a real multi-container pod: N
containers sharing the pod's netns (same IP, ``localhost`` between them), IPC
and UTS — managed as a unit with ``delonix pod``.

``kind: Workload`` (``spec.type: container | vm | pod | microvm``) is a single
declarative object for both compute types: it lowers to the matching Kind at
load time (``type: microvm`` pins the Cloud Hypervisor backend), so one schema
covers everything. The imperative day-2 side is ``delonix workload
ls/describe/stop/rm``, which routes by name across containers and VMs.

.. code-block:: yaml

   apiVersion: delonix.io/v1
   kind: Network
   metadata: { name: backend }
   ---
   apiVersion: delonix.io/v1
   kind: Container
   metadata: { name: db }
   spec:
     image: postgres:16-alpine
     network: backend
     volumes: [ "data:/var/lib/postgresql/data" ]
     ports: [ "5432:5432" ]
   ---
   apiVersion: delonix.io/v1
   kind: FirewallPolicy          # L4 firewall, k8s-NetworkPolicy style
   metadata: { name: db-in }
   spec:
     target: db
     direction: ingress
     defaultPolicy: deny
     rules:
       - { proto: tcp, port: "5432", from: 10.219.0.0/16 }

Architecture
============

Ten crates, one binary, no residing process:

.. list-table::
   :header-rows: 1
   :widths: 26 74

   * - Crate
     - Responsibility
   * - ``delonix-runtime-core``
     - Shared types: ``Container``, ``Vm``, ``Status`` (6-state), ``Store``, the secret vault.
   * - ``delonix-runtime`` / ``-bin``
     - The runtime (clone/namespaces/cgroups, create/stop/exec, reconcile) + the ``delonix`` CLI.
   * - ``delonix-net``
     - Rootless SDN: holder netns + bridge + single slirp, nft DNAT/firewall, internal DNS, WireGuard overlay, and the eBPF flow datapath.
   * - ``delonix-image``
     - OCI images: pull (digest-verified), build, export, buildpacks, signatures, internal registry.
   * - ``delonix-vm``
     - Declarative microVMs (``VmBackend``: Cloud Hypervisor / libvirt), cloud-init.
   * - ``delonix-volume``
     - Named volumes, bind mounts, quotas, network drivers (NFS/CIFS/WebDAV).
   * - ``delonix-cri``
     - CRI ``runtime.v1`` server — the kubelet talks to Delonix.
   * - ``delonix-mgmt``
     - Management API (HTTP+JSON over a unix socket) for external control-planes, plus the shared Prometheus registry and OpenTelemetry spans.
   * - ``delonix-scan``
     - SBOM + CVE scanning (``image scan`` and scan-on-pull enforcement).

See the `architecture page
<https://angolardevops.github.io/delonix-runtime/arquitectura.html>`_ and the
`C4 model <https://angolardevops.github.io/delonix-runtime/c4.html>`_ for the
full picture.

Appendix — features by release
==============================

The complete, always-current changelog lives in
`docs/RELEASES.md <docs/RELEASES.md>`_ — one section per release, newest
first, **regenerated automatically by the release pipeline** on every
published tag (source of truth: ``docs/releases/<tag>.md``, the same notes
published on GitHub Releases).

License
=======

Apache-2.0. See `LICENSE <LICENSE>`_.
