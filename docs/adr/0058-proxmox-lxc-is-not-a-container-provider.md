# ADR-0058: A Proxmox LXC container is not a provider of `kind: Container`

- **Status:** Proposed
- **Date:** 2026-09-27
- **Deciders:** Walter Angolar
- **Related:** ADR-0049 D4 (LXC gets "an ADR of its own, after a spike, or it stays excluded";
  this is that ADR, and the spike is below); ADR-0050 (the capability catalogue, whose
  `container.*` rows are the contract a provider of `Container` would have to answer); ADR-0044
  (the provider ports); ADR-0008 (the Proxmox backend addresses one node and never picks one).

## Context

The Proxmox VE API has 62 routes under `/nodes/{node}/lxc`, and ADR-0049's route matrix marks all
62 `unsupported-by-design` with one reason: a Proxmox LXC container would be the first container
served by a REMOTE provider, and whether it is a `Container` at all is a boundary question, not a
slice. ADR-0049 D4 asked three things of a spike: is it a `Container` Kind; which capabilities it
can and cannot honour (namespaces, the SDN, `exec`, logs); and which `pct` semantics leak through
the API.

**The spike.** It ran on 2026-09-27 against the lab node `pve` (PVE 9.2.2, kernel 7.0.2-6-pve,
the appliance this repository builds). It used `pvesh` on the node, which calls the same API
handlers in-process, so it produced no route trace and promotes no row of the matrix. Two sources
were read: the node's own schema (`pvesh usage`, and `docs/proxmox/api-9.2.2.routes.json`), and
three containers created from `docker.io/library/alpine:3.20` (the containers, their volumes and
the template were removed afterwards; the node was left as it was found).

**What works, measured:**

- **An OCI image becomes a container.** `POST /nodes/{node}/storage/{storage}/oci-registry-pull`
  is a task with a UPID (11 s for alpine, 3.47 MB), and it writes an OCI archive into the
  storage's `vztmpl` content. `POST /nodes/{node}/lxc` with that archive as `ostemplate` reports
  "Detected OCI archive" and fills `entrypoint` (from the image's `CMD`), `env`, `cmode: console`,
  `lxc.init.cwd` and `lxc.signal.halt` from the image config.
- **Unprivileged by default.** `unprivileged: 1`, uid 0 inside mapped to 100000 on the node
  (`uid_map 0 100000 65536`). The LXC runtime itself runs as root on the node.
- **Lifecycle, limits and snapshots** are tasks like the QEMU ones: `vzcreate`, `vzstart`,
  `vzsnapshot`, `vzdestroy`. `memory: 128` became `memory.max = 134217728` and `swap: 0` became
  `memory.swap.max = 0` in the container's cgroup. A snapshot on `local-lvm` works
  (`GET …/feature?feature=snapshot` → `hasFeature: 1`).
- **An image the engine pulled itself can be sent to the node.** `POST …/storage/{storage}/upload`
  with `content=vztmpl`, `checksum-algorithm=sha256` and the file's sha256 answered 200 with an
  `imgcopy` task that ended `OK`, and kept the file under the exact name sent
  (`dlx-alpine-bf8527eb54c3.tar`). The same route with `content=import` is what ADR-0057 uses for
  VM disks, and its record says a checksum mismatch fails the task.
- **But only with OCI media types.** `delonix image save` keeps the manifest a registry served,
  and for `alpine` that is Docker v2 (`application/vnd.docker.distribution.manifest.v2+json`,
  config `vnd.docker.container.image.v1+json`). The node refuses that archive with `Error while
  parsing OCI image: Unsupported CPU architecture`, a message that names the wrong cause: the
  config blob is the same one the node's own pull fetched (`bf8527eb…`, `architecture: amd64`).
  The same archive rewritten with OCI media types, and nothing else changed, was accepted
  ("Detected OCI archive", `arch amd64`) and the container started. The control, the unmodified
  archive, was refused again in the same run.

**What does not, measured:**

1. **No `exec`.** The QEMU side has `agent/exec`; the LXC side has none. The only ways in are
   interactive consoles over a websocket (`termproxy`, `vncproxy`, `spiceproxy`). Running a
   command and getting its exit status is not in the API.
2. **No logs.** No route returns the output of the container's process, and the node keeps none
   (`/var/log/lxc/` is empty; `cmode: console` sends it to a console nobody reads).
3. **No exit status.** An entrypoint that exits 7 after 3 s leaves the container `stopped`, with
   no exit field in `status/current`. The `vzstart` task says `OK`, and systemd logs
   `pve-container@102.service: Deactivated successfully`. The 7 is gone.
4. **`entrypoint` and `env` are silently replaced on create.** A create from an OCI archive with
   `--entrypoint /bin/true --env FOO=bar` stored `entrypoint: /bin/sh` and the image's `PATH`,
   with `FOO` dropped. The task said `OK`. A `PUT …/config` afterwards is stored as sent.
5. **A network that fails ends the start in a third state.** With `net0: ip=dhcp` the node
   enables "host-managed" networking (the image has no DHCP client) and runs `dhclient` itself.
   With no offer, the `vzstart` task ends with `exitstatus: "WARNINGS: 1"` — neither `OK` nor an
   error — and its log (`GET /nodes/{node}/tasks/{upid}/log`) carries the reason on a `WARN:` line
   (`DHCP failed … exit code 2`). The container runs with no IPv4 address (`GET …/interfaces`:
   `eth0` with only a link-local address). The engine's `task_verdict` reads every exit status
   other than `OK` as a failure, so today it would report this start as FAILED with the container
   running; the verdict is shared by every task this backend waits for, QEMU included.
6. **The node's own pull takes a tag, and the digest is lost.** The `reference` parameter of
   `oci-registry-pull` must end in `:<tag>`, per the node's schema; a `@sha256:` reference does not
   match it. The stored file is named `alpine_3.20.tar`, with no registry, no repository path and
   no digest. Two images with the same last path component and tag map to one file name. The
   `digest` field in the container config is a SHA-1 of the config, not the image's. The pull has
   two parameters, `reference` and `filename`, and no credentials: a registry that needs a login
   cannot be pulled from through the API. And `filename` is rewritten: `dlx/../Alpine sha256:ab.tar`
   became `Alpine_sha256_ab.tar.tar` (the path dropped, characters replaced, `.tar` appended). All
   of this is the pull route only; the upload route above avoids it.
7. **Every container is a full copy.** The archive is extracted into a new volume per container
   (`rootfs: local-lvm:vm-100-disk-0,size=1G`). There is no layer sharing.
8. **Nothing of this engine's dataplane applies.** The container sits on a bridge of the remote
   node (`vmbr0`). The SDN, namespace isolation, internal DNS, `publish` and the per-workload
   firewall chains of this engine never see it. The node's own firewall does have per-container
   routes (`…/lxc/{vmid}/firewall/*`), mirroring the per-VM ones of ADR-0052.

## Decision

1. **A Proxmox LXC container does not serve `kind: Container`.** `Container` is this engine's
   rootless container over the kernel, and consumers use it through `exec`, `logs`, `wait`,
   `run` in the foreground, digest-pinned images, the SDN and namespace isolation. Points 1, 2, 3
   and 8 above are not gaps a client can close; the API does not have them. (Point 6 is: the
   engine can pull and verify the image itself and upload it.) A provider of
   `Container` that declares most of `container.*` `unsupported-by-provider` would make every
   caller check for capabilities that the Kind exists to guarantee.

2. **If LXC enters, it is a separate resource with VM-like semantics.** What the API offers is a
   **system container**: a persistent root volume, an init, lifecycle and snapshot tasks, clone,
   template, migration, backup and a per-guest firewall. It is closer to a `VirtualMachine` than
   to an OCI application container. Such a resource would sit behind the compute provider port
   (ADR-0044), with its own Kind and its own capability rows. It is not built now: no need for it
   has been named, and ADR-0049 D5 builds slices against a need, not against a route count.

3. **Whoever builds it later keeps the six measured traps as contract rules.** They are written
   here so they are not rediscovered one incident at a time:
   - `entrypoint`/`env` are set by `PUT …/config` after the create and read back, never trusted
     from the create call (point 4).
   - A task has three outcomes, not two: `OK`, an error, and `WARNINGS: <n>`, whose `WARN:` lines
     are read from the task log and reported. A start with warnings is not a failure and not a
     silent success; the network is judged by `GET …/interfaces` (point 5).
   - The image is pulled by the engine, not by the node: digest-verified, with the engine's own
     registry credentials, written as an archive with OCI media types, and sent with its sha256
     through the upload route. The node's `oci-registry-pull` is not used; its file name is not an
     identity (point 6).
   - `exec`, logs and exit status are declared `unsupported-by-provider` with the reason, never
     emulated through a console websocket (points 1–3).
   - The full copy per container is stated in the capability detail (point 7).
   - Pulls, creates and starts go through the existing task ledger (ADR-0049 slice 1).

4. **The 62 routes stay `unsupported-by-design`.** Their reason text keeps pointing at ADR-0049
   D4 while this record is Proposed; on acceptance, the inventory reason moves to this ADR
   (`scripts/proxmox_api_inventory.py`, regenerated with the matrix).

## Alternatives considered

- **Proxmox LXC as a provider of `kind: Container`, with most capabilities refused by name.** It
  would honour the catalogue's letter (ADR-0050: nothing hidden, every "no" with a reason) and
  miss its purpose: a `Container` that cannot `exec`, log, report an exit code or pin an image by
  digest is not something a compose file, the CRI or the Docker API slice can run. Every consumer
  of `Container` would have to branch on the provider, which is the `if provider == …` the
  engine's boundary forbids. Rejected.
- **Emulate `exec` and logs through `termproxy`.** A console websocket is an interactive TTY. It
  has no exit status and no stream separation, and it is shared with anyone else attached. Output
  scraped from it would look like logs and not be logs. Rejected (the no-silent-failure
  guardrail).
- **Reach the container through SSH to the node and `pct exec`/`lxc-attach`.** It would give
  `exec`, and it is outside the API: it needs a root shell on the hypervisor, which ADR-0049 D3
  and ADR-0010 keep out of this engine. Rejected.
- **Build the system-container Kind now.** It is the right shape (decision 2), and the spike
  shows the API supports it. It is not built without a named need; the cost is a Kind, a
  provider port implementation, capability rows and a live case per operation.
- **Keep LXC excluded with no ADR.** This leaves ADR-0049 D4's question open, and the next
  person repeats the spike. Rejected in favour of recording what was measured.

## Consequences

- The question ADR-0049 D4 asked has an answer with evidence, and the routes that stay excluded
  point at a reason that is measured, not assumed.
- `kind: Container` keeps one meaning. No consumer needs to know which provider served a
  container, because only the kernel provider does.
- The six traps are recorded before any code exists, which is when they are cheapest to design
  around. Two of them (points 4 and 5) also apply to the QEMU create path whenever it accepts a
  field the node may overwrite or a warning it may downgrade, and are worth checking there.
- The implementation plan, slice by slice, is `docs/discovery/63_PROXMOX_LXC_PLANO.md`.
- **Not validated:** most routes were exercised through `pvesh` on the node, not through
  `delonix-proxmox` over HTTPS, so the matrix does not change; the upload was a `curl` on the
  node with an API token created and deleted there. A digest-only reference WAS sent to the live
  node and refused (`400 … does not match the regex pattern`). Not tried: a privileged container
  (`unprivileged: 0`), `features: nesting=1`, a network where DHCP answers, and a `vztmpl` upload
  with a wrong checksum (the request never produced a task, for a reason not isolated). Whether
  QEMU tasks end in `WARNINGS` in practice was not measured; the verdict that would misread them
  is shared code.
