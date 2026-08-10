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

:Version: 0.45.0
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
     - none — a short-lived supervisor per container (same model as ``conmon``),
       which is what makes a detached container's exit code knowable
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
     - own CRI + ``kubeadm`` bootstrap from scratch (``delonix cluster``).
       Conformance is **measured and published**: 79/103 specs of the upstream
       ``critest`` — see `Kubernetes CRI conformance`_
   * - Health checks
     - continuous
     - continuous (needs systemd timers)
     - continuous, **without systemd** — the detached container's own supervisor
       runs the probe
   * - Custom seccomp profiles
     - yes
     - yes
     - yes (OCI JSON), and a profile it cannot fully express is **refused**,
       never silently downgraded
   * - Firewall
     - basic
     - basic
     - per-container L4 (``ingress``/``egress``) + declarative ``kind: FirewallPolicy``
   * - Observability
     - ``stats``
     - ``stats``
     - eBPF per-container flow accounting (``delonix net flow``)
   * - Resource limits, rootless
     - n/a (daemon is root)
     - needs a delegated cgroup
     - needs a delegated cgroup — see `Resource limits need a delegated cgroup`_

Resource limits need a delegated cgroup
=======================================

``-m`` / ``--cpus`` / ``--pids-limit`` only take effect when the process that
starts the container owns a **delegated cgroup**. From an ordinary SSH session
they are silently inert — and that is a cgroup v2 rule, not a limitation of this
engine: ``rootless Podman has exactly the same requirement``.

Measured on a clean Ubuntu 24.04 VM over plain SSH, with ``-m 128M --cpus 0.5``::

    cgroup: /user.slice/user-1000.slice/session-40.scope   (shared with sshd)
    memory.max=max   cpu.max=max   pids.max=max

The reason is that an SSH session scope is a **sibling** of
``user@<uid>.service``, not a child, and moving a process between them needs
write access to their common ancestor ``user-<uid>.slice``, which belongs to
root. Namespace and seccomp isolation are unaffected — only the resource
ceilings.

``delonix system setup`` diagnoses it. It gives **two** remedies, in this
order, because most answers online jump straight to the second one and it is
usually not the one you need:

**1. A delegated scope.** No root, no reboot, works in the shell you are
typing in. Try this first — on a distro that already delegates ``cpu`` (Ubuntu
24.04 ships ``Delegate=pids memory cpu`` on ``user@.service``) it is the whole
fix, and editing ``/etc`` would only restate what is already there::

    systemd-run --user --scope -p Delegate=yes -- delonix container run -d -m 512M myapp

…or, for anything long-lived, from a systemd **user** unit, which already gets a
delegated cgroup::

    [Service]
    Delegate=yes
    ExecStart=/usr/local/bin/delonix container run ...

**2. A drop-in on** ``user@.service`` (``sudo delonix system setup --delegate``).
Needs root, survives reboots, and is only worth writing when step 1 still
reports ``cpu`` missing — that means the distro itself does not delegate it.
Note it fixes *future* logins, not the shell you are in: an SSH
``session-N.scope`` is a **sibling** of ``user@.service`` and inherits nothing
from it, so you still enter a scope (or log out and back in) afterwards.

``cpuset`` and ``io`` are a separate matter: on a stock Ubuntu the root-owned
``user.slice`` passes only ``cpu memory pids`` down, so no drop-in of yours can
make them appear. Nothing in this engine needs them — ``system setup`` lists
them as *absent*, not *missing*.

Verify it took effect — this is the check worth putting in your provisioning::

    systemd-run --user --scope -p Delegate=yes -- \
      delonix container run -d --name t -m 128M alpine sleep 60
    # memory.max must read 134217728, not "max"

With delegation in place the engine applies the whole set: the per-container
``memory.max`` / ``cpu.max`` / ``pids.max``, ``memory.swap.max=0`` (so a limit
bounds real memory and not just the resident set), ``memory.oom.group=1`` (so an
OOM kills the container, not one process inside it), **and** an aggregate ceiling
on the parent sized from the host — the thing that stops N containers, none of
which carry ``-m``, from summing to more than the machine has.

Golden VM images ship known credentials
=======================================

The golden VM images (``delonix vm pull`` / ``delonix image vm build``) are
built with a **fixed, publicly known password**: ``root`` and a ``delonix``
user, both with the password ``delonix``, and ``delonix`` has passwordless
``sudo``. They are in the build recipe in this repository, so treat them as
public knowledge, not as a secret.

They exist so that a VM whose network never came up is still reachable from the
serial console (``delonix vm console <name>``). Everything else authenticates
with keys: cloud-init injects your ``--ssh-key`` on first boot, and
``delonix cluster kubeadm`` generates and uses its own.

Because of that, the images ship with **SSH password login disabled**
(``PasswordAuthentication no``, ``PermitRootLogin prohibit-password``) — the
password works on the console, not over the network. If you run one of these
images anywhere reachable, still do the obvious thing::

    # inside the VM, on first login
    sudo passwd root
    sudo passwd delonix

Or build your own golden with your own accounts::

    delonix image vm build --extra-run "passwd -l root" ...

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
- **Network storage.** A ``kind: Volume`` with an ``nfs:``/``cifs:``/``webdav:``
  block mounts a share from a NAS (TrueNAS/Synology/Samba/Nextcloud) as a named
  volume, k8s-PersistentVolume style. (``kind: Storage`` still loads, rewritten
  into exactly this with a deprecation warning.)
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
  node runtime *is* Delonix. Conformance is measured, not asserted — see
  `Kubernetes CRI conformance`_.
- **Health checks without a daemon.** ``--health-cmd`` and friends, with the
  ``STATUS`` column showing ``Up 21 seconds (healthy)``. The probe is run by the
  detached container's **own supervisor** — no fleet-wide process, and no
  systemd timers (which rootless Podman needs for the same thing). ``run
  --wait`` blocks until the image's ``HEALTHCHECK`` passes, replacing the
  ``until curl …; do sleep; done`` everyone ends up writing badly.
- **Custom seccomp profiles.** ``--security-opt seccomp=<profile.json>`` in the
  OCI/runc format, and Kubernetes' ``localhostProfile`` through the CRI. Syscall
  names resolve **per architecture** rather than from a table of numbers, and a
  profile this engine cannot express exactly — argument-filtered rules, for
  instance — is refused rather than approximated into a weaker one.
- **Short paths for the hot verbs.** ``delonix ps``, ``run``, ``exec``,
  ``logs``, ``rm``, ``images``. They are the same commands, reached by rewriting
  argv, so they cannot drift from the grouped form.

Kubernetes CRI conformance
==========================

``delonix-cri`` is measured against **cri-tools ``critest``**, the upstream
suite, and the number is published rather than claimed: *serves a kubelet* is an
assertion, *79 of 103 named specs* is a fact somebody else can check.

::

    Ran 103 of 122 Specs
    79 Passed | 24 Failed | 19 Skipped        # rootless, cgroup v2

Reproduce with ``tests/compat/cri-conformance.sh``. The full breakdown of what
fails and why — including what is **not** ours — is in
`docs/cri-conformance.md <docs/cri-conformance.md>`_.

Of the remaining failures, roughly half are not engine gaps: nine are AppArmor
specs, which need ``CAP_MAC_ADMIN`` in the *initial* user namespace (Docker and
containerd have exactly the same limit), and four are mount tests where the
suite itself cannot mount on the host without root.

One divergence is **deliberate** and will not change to win a spec: a container
with no seccomp profile declared runs under the engine's built-in allowlist, not
unconfined. That is stricter than the specification asks for.

There is also a Docker Engine API compatibility layer, whose coverage is
published the same way — ``delonix serve docker-api --matrix`` prints the routes
that exist and the ones that deliberately do not, with the reason for each.

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
