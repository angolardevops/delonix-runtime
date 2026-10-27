# Why delonix — the launch narrative

> One binary that runs containers, microVMs, pods and Kubernetes — daemonless,
> rootless, and honest about what it does and does not do. Every number below is
> measured on a real host; where a claim is a ceiling, it says so.

## The one-sentence pitch

**delonix is a daemonless, rootless container-and-VM runtime that the ecosystem's
own tools already drive** — `docker`, Testcontainers, `kubectl`/CRI,
`docker-compose` — with a published capability matrix where every "supported"
cites evidence you can run.

The pitch is **not** "we are Docker". Chasing Docker feature-for-feature is a
race with no prize. The pitch is the combination no one else ships in a single
rootless binary, plus a discipline — honesty — that the incumbents do not offer.

## Four things that are true, and measured

### 1. Daemonless and rootless, by default — and faster for it

No resident daemon, no socket to a privileged service, no group to join.

- `run --rm` latency, each engine on its own default (median of 10, measured
  2026-08-13, VM `pari053`): **delonix 91 ms · Docker 208 ms · Podman 268 ms**.
  It is architecture, not tuning: `run` is a process that `clone()`s and exits.
- Reconfigure a live container WITHOUT recreating it: `update --publish-add`
  changes ports keeping the same PID — Docker and Podman answer `unknown flag`.

### 2. Containers AND microVMs AND pods AND Kubernetes — one binary

| | delonix | docker | podman |
|---|---|---|---|
| Containers / images / networks / volumes | ✅ | ✅ | ✅ |
| Pods | ✅ `pod` | ❌ | ✅ |
| Compose (embedded, no plugin) | ✅ | ⚠️ plugin | ⚠️ external |
| microVMs as a workload (libvirt / Cloud-Hypervisor / Proxmox) | ✅ `vm` | ❌ | ⚠️ host-VM only |
| Kubernetes: CRI + kubeadm + `apply`/`get` | ✅ | ⚠️ Desktop | ⚠️ `kube play` |
| GitOps / declarative (`apply`/`plan`/`drift`) | ✅ | ❌ | ❌ |

### 3. The ecosystem's tools already drive it

- **Testcontainers runs end to end** against `delonix serve docker-api` — proven
  with the real testcontainers-python on docker-py 7.2.0: `start`,
  `get_exposed_port`, `wait_for_logs`, `exec`, `get_logs`, `stop` — all green.
  Including **Ryuk, its reaper**, which pulls and runs and cleans up by label.
  See `tests/compat/docker_api_smoke.py` and `tests/compat/ryuk_reap.py`.
- **And safer than on Docker.** On Docker the daemon is root, so a container
  with the socket bound in is host-root — the canonical escape. On delonix the
  socket peer is the unprivileged engine user: a socket-bearing container can do
  only what that rootless user can, never touch host root. The most dangerous
  bind mount in the container world stops being a privilege escalation.
- **Kubernetes:** a real CRI (`delonix serve cri`), measured against upstream
  `critest` (cri-tools v1.36.0), and `kubeadm` clusters with a CNI (flannel).
- `delonix compatibility docker` / `compatibility compose` print, route by route
  and key by key, what is served and what is refused — with the reason.

### 4. Honesty is a feature, not a footnote

- `delonix provider matrix` publishes a capability matrix where **every
  `supported` cell cites evidence that exists** (a battery, a scenario, a test),
  enforced by a gate. Today: **180 of 270 cells proved**. The honest ceiling is
  **~247** — the rest are provider limits or require infrastructure, and the doc
  says which. Nobody else in this space ships this.
- "Never a silent gap": a capability is served, refused with a reason, or
  declared unavailable — never quietly broken.

## What it is NOT (say it first, before a user finds out)

- Not a Docker Desktop; Linux-first, no Mac/Windows GUI.
- Not 100% Docker API — it serves the slice real tooling calls (measured), and
  refuses the rest with a reason.
- The capability matrix is at 180/270, not 270/270 — and the ceiling is ~247,
  not 270. The number is honest precisely because it is not round.

## The demo that lands

1. `delonix run` an nginx, show no daemon and sub-100 ms start.
2. `delonix vm create` a microVM next to it — same binary, same CLI.
3. Point Testcontainers at it (`DOCKER_HOST=unix://…`), run a suite green —
   Ryuk and all — then note the socket is not host-root here.
4. `delonix provider matrix | head` — the honesty, on screen.
