# 8. Delonixfile and VMfile

Delonix has two build files, and they look alike on purpose: anyone who has written a Dockerfile
can read both. What they build is different.

- A **Delonixfile** builds an **OCI container image** (layers of a filesystem). It is a Dockerfile
  grammar with a few Delonix instructions on top, built by `delonix build`.
- A **VMfile** builds a **bootable qcow2 disk** for a VM. It borrows the Dockerfile *shape*, but the
  mechanism is `qemu-img` + `virt-customize` on a whole disk, built by `delonix image vm build`.

This page describes what the parsers in this repository actually accept — not what Docker accepts.
Every rule below points at the code that enforces it.

> Examples marked *parse-checked* were run against a binary built from this tree, with
> `DELONIX_ROOT`, `DELONIX_NET_RUNTIME_DIR` and `TMPDIR` pointed at a scratch directory, up to the
> point where the build would start pulling images or downloading base disks. Full builds were
> **not executed in this review** (they need registry/network access and, for VMs, libguestfs and
> several GB of RAM and disk).

---

## Part 1 — Delonixfile

### Where the code lives

| Concern | File | Symbol |
|---|---|---|
| Grammar (parser) | `crates/adapters/delonix-oci/src/build.rs` | `parse_dockerfile_with_args`, `parse_run_flags`, `parse_secret_mount`, `resolve_target_stage` |
| Build orchestration | `bins/delonix-runtime-bin/src/cmd/build.rs` | `run`, `build_from_spec`, `build_one_stage`, `default_build_file` |
| Image commit | `crates/adapters/delonix-oci/src/build.rs` | `ImageStore::commit_flat_rootfs` (rootless), `commit_upper` + `build_image` (root) |
| Project templates | `bins/delonix-runtime-bin/templates/<name>/Delonixfile` | rendered by `delonix init` / `stack init` |

### File lookup

`delonix build [CONTEXT]` without `-f` calls `default_build_file` (`cmd/build.rs`): it uses
`<context>/Delonixfile` if it exists, otherwise `<context>/Dockerfile`. **The grammar is the same
for both names** — the Delonix instructions below are accepted in a file called `Dockerfile` too.
`Delonixfile` is just the name that is discovered first. `-t/--tag` is required.

```bash
delonix build -t myapp:dev .                 # Delonixfile, else Dockerfile
delonix build -t myapp:dev -f build/Other .  # explicit file
```

### Instructions the parser accepts

`parse_dockerfile_with_args` is **fail-closed**: an instruction it does not know is an error with
the line number, and any instruction except `ARG` before the first `FROM` is an error. Lines ending
in `\` are joined; lines starting with `#` are comments. Instruction names are case-insensitive.

| Instruction | What happens | Notes |
|---|---|---|
| `ARG NAME[=default]` | Declares a build variable; `${NAME}`/`$NAME` is substituted in every later line | Allowed before `FROM` (to parameterize it). `--build-arg NAME=VALUE` overrides only a declared `ARG`. Simplification: args live in **one** scope for the whole file, not per stage. No `${NAME:-default}` forms (`substitute_vars`). |
| `FROM <image> [AS <name>]` | Opens a stage | A later stage may also say `FROM <earlier-stage>` (see multi-stage). |
| `RUN <shell>` | Runs in a working container via `exec` | Only `--mount=type=secret,...` is accepted as a flag (below). |
| `COPY [--from=<stage>] <src> <dst>` | Writes into the stage's rootfs on disk | Confined to the context/rootfs (`safe_join`, `confine_to`): `..`, absolute escapes and symlinks that leave the base are refused. |
| `ADD` | **Same as `COPY`** | No URL download, no automatic archive extraction. |
| `ENV K=V [K2="v 2" …]` or `ENV K V` | Affects later `RUN`s; final stage's `ENV` goes into the image config | Values are expanded against earlier `ENV`s (`expand_env_value`). |
| `WORKDIR <dir>` | Working dir of later `RUN`s; final value goes into the image config | |
| `USER <name|uid[:gid]>` | Recorded in the image config | Empty → inherits the base image's. |
| `CMD`, `ENTRYPOINT` | Recorded in the image config | Exec (JSON) and shell forms. |
| `HEALTHCHECK [opts] CMD <cmd>` / `HEALTHCHECK NONE` | The part after `CMD` is stored as the image's health command | Options before `CMD` are ignored. Used by `delonix container healthcheck <id>` and by compose's `depends_on: condition: service_healthy`. |
| `LABEL`, `EXPOSE`, `MAINTAINER`, `VOLUME`, `STOPSIGNAL`, `SHELL`, `ONBUILD` | **Accepted and ignored** | Metadata only; no build effect. |

**Delonix extensions** (parsed into `Dockerfile` fields in the same file):

| Instruction | Parsed into | Effect in this tree |
|---|---|---|
| `SCAN fail-on=<sev>` | `scan_fail_on` (default `high` if no `fail-on=`) | **Parsed but not enforced by `delonix build`** — no caller reads the field. Gate images explicitly with `delonix image scan --fail-on <sev> <image>`. |
| `CPUS <n>` | `cpus` | Written to the image config (non-standard `Cpus` key); inherited from the base if absent. No run-time consumer was found in this review. |
| `MEMORY <n>` | `memory` | Same as `CPUS` (`Memory` key). |
| `SECURITY <opt>...` | `security` | Same as `CPUS` (`Security` key). |

Treat `CPUS`/`MEMORY`/`SECURITY` as *recorded intent*; set real limits at run time
(`container run -m/--cpus`, or `resources` in a manifest). If you wire them up, update this table.

Parser behaviour worth knowing (read in the code, not a design promise):

- `COPY a b c /dst` keeps only the **first** source and the **last** argument; middle sources are
  dropped silently.
- `COPY --chown=…`/`--chmod=…` are not recognized; the flag token would be taken as the source path.
- `RUN --network=…`, `RUN --security=…` and any other `RUN --<flag>` are refused.

### Multi-stage, `COPY --from` and `--target`

Each stage (the final one included) gets its **own working container and rootfs**
(`build_one_stage`). The working container (`sleep infinity` over the stage's rootfs) is created
by `ensure_container` through the same `WorkloadRuntime` that `container run` and `start` use
(`container::with_host_workload`), so a build cannot drift from a container's spawn specification. Intermediate stages stay on disk until the whole build ends, so:

- `COPY --from=<name-or-index> <src> <dst>` reads straight out of that stage's rootfs
  (`resolve_copy_source`, `copy_into_rootfs`).
- `FROM <earlier-stage>` clones that stage's rootfs with `cp -a --reflink=auto`
  (`clone_rootfs`) — `cp -a` so symlinks such as `/bin -> usr/bin` stay symlinks.
- `--target <name-or-index>` (`resolve_target_stage`) stops at that stage and packages it; later
  stages are not built. An unknown name is refused and the error lists the stages that exist.
  With `--target` on an intermediate stage, `CMD`/`ENTRYPOINT`/`USER`/`ENV`/`WORKDIR`/`HEALTHCHECK`
  come from **that** stage, not from the file's final stage.

Two root-mode (overlay) restrictions, both refused up front with a clear message: the final stage
cannot be `FROM <earlier-stage>` (the OCI commit needs a real base image for lineage), and
`--target` on an intermediate stage is refused. Rootless has neither restriction.

### Build secrets

```bash
delonix build -t app:dev --secret id=npmrc,src=$HOME/.npmrc .
```

```dockerfile
RUN --mount=type=secret,id=npmrc,target=/root/.npmrc npm ci
```

- `--secret id=<name>,src=<path>` is repeatable; a malformed entry or a missing `src` file is a hard
  error (`parse_build_secrets`).
- In the file: `--mount=type=secret,id=<name>[,target=<path>][,required=true|false]`. `target`
  defaults to `/run/secrets/<id>`; `required` defaults to `false` (a missing optional secret is
  skipped, like Docker).
- `type=ssh`, `type=cache` and `type=bind` are **refused** (`parse_secret_mount`).
- The secret is bind-mounted live (`mount_run_secrets`) inside the working container's mount
  namespace only for that one `RUN`, then unmounted — the host-side view of the rootfs that the
  commit and the layer cache read never contains it. The secret material is also not hashed into
  the cache key.

### `--platform` and binfmt

`--platform linux/<arch>` (`parse_platform`: only `linux/` is accepted) resolves the base image
for that architecture and stamps the arch into the result. **Running** a foreign-arch `RUN` needs
the host's own `binfmt_misc` + `qemu-user-static` registration, which Delonix does not manage.
`build_from_spec` checks `/proc/sys/fs/binfmt_misc/qemu-<arch>` before building and refuses with
the interpreter name if it is missing or disabled.

### Rootless vs root, and the layer cache

| | Rootless (normal path) | Root |
|---|---|---|
| Rootfs of a stage | flat directory (`prepare_rootfs_flat`) | overlay |
| Commit | `commit_flat_rootfs` — one squashed layer on top of the base's layers | `commit_upper` (tar of the upperdir) + `build_image` |
| Layer cache | **yes** | **never** |

The cache (`<DELONIX_ROOT>/build-cache/<hash>/rootfs`, `--no-cache` to bypass) is a rolling hash
chain, one link per instruction. `RUN`/`COPY` snapshot the full rootfs after running; `ENV`/`WORKDIR`
fold into the chain without a snapshot. A `COPY` link hashes the **bytes** being copied, so a changed
file invalidates everything after it. On a hit, the cached snapshot is cloned into a fresh
container's rootfs — never synced onto a live one (that corrupted the `/proc`/`/sys`/`/dev` mounts
and was abandoned). Trade-offs stated in the module doc: full-rootfs snapshots, not per-layer diffs
(mitigated by `--reflink=auto`), and **no cache GC** — `build-cache/` only grows.

Because every `RUN` executes in a real container, the host prerequisites of
[01 — Preparing your environment](01-environment.md) (user namespaces, subuid/subgid, AppArmor on
Ubuntu) apply to builds too.

### Worked example: a template

`delonix init -t <template> [DIR]` (or `delonix stack init --template <template>`) renders
`bins/delonix-runtime-bin/templates/<template>/`, replacing `__NAME__`, `__PORT__` and
`__TEMPLATE_VERSION__` (the `version=` default in `template.meta`, or `-v/--template-version`).
In an empty directory it writes the full project; in a non-empty one it only adds the Delonix glue
(Delonixfile, manifest, CI files). Generated in a scratch directory from `httpd` (*run*):

```bash
$ delonix init -t httpd web
detected an empty directory → stack init --template httpd
  created: web/.dockerignore
  created: web/Delonixfile
  ...
```

```dockerfile
# Delonixfile — production-ready Apache httpd.
# Build with:  delonix build -t web:dev .
FROM httpd:2.4-alpine
# Listen on 8080 and harden a little (no version banner).
RUN sed -i 's/^Listen 80$/Listen 8080/' /usr/local/apache2/conf/httpd.conf && \
    printf '\nServerTokens Prod\nServerSignature Off\nTraceEnable Off\n' >> /usr/local/apache2/conf/httpd.conf
COPY public /usr/local/apache2/htdocs
EXPOSE 8080
HEALTHCHECK CMD wget -qO- http://127.0.0.1:8080/healthz || exit 1
CMD ["httpd-foreground"]
```

The generated `delonix-manifest.yaml` passes `delonix manifest validate` (*run*). `--up` would build,
`stack apply` and wait for health — not executed here.

A multi-stage file using most of the grammar, *parse-checked* (the `--target nosuch` error proves
the file parsed and lists its stages, before any pull):

```dockerfile
ARG GO_VERSION=1.23
FROM golang:${GO_VERSION}-alpine AS builder
WORKDIR /src
COPY go.mod ./
RUN --mount=type=secret,id=netrc,target=/root/.netrc go mod download
COPY . .
RUN CGO_ENABLED=0 go build -o /out/app ./cmd/app

FROM alpine:3.20
COPY --from=builder /out/app /usr/local/bin/app
USER 65534
HEALTHCHECK CMD wget -qO- http://127.0.0.1:8080/healthz || exit 1
CMD ["/usr/local/bin/app"]
```

```text
$ delonix build -t demo:dev --target nosuch .
error invalid argument: no stage named 'nosuch' in this Dockerfile — known stages: builder
$ delonix build -t d -f Dockerfile.cache .        # RUN --mount=type=cache,...
error invalid argument: RUN --mount=type=cache: só type=secret é suportado (ssh/cache/bind ainda não)
$ delonix build -t d --platform windows/amd64 .
error invalid argument: --platform 'windows/amd64': only 'linux/<arch>' is supported (this engine does not run another OS)
```

(Some parser errors are still in Portuguese; they count as LANG-01 debt — see
[10 — Contributing workflow](10-contributing-workflow.md).)

### The repository's own `Delonixfile` is not built by `delonix`

The `Delonixfile` at the repository root packages the `delonix` CLI into a container image. It is
built with **Docker or Podman** (`docker build -f Delonixfile …`, or `make image`), not with
`delonix build`: it uses `# syntax=docker/dockerfile:1` and `RUN --mount=type=cache`, which
`delonix build` refuses. Do not use it as an example of the Delonix grammar; use the templates.

---

## Part 2 — VMfile

### Where the code lives

| Concern | File | Symbol |
|---|---|---|
| Grammar, scaffold, builder | `bins/delonix-runtime-bin/src/cmd/vmfile.rs` | `parse`, `classify_base`, `resolve_base`, `stage_ops`, `build`, `finalize`, `scaffold` |
| CLI entry, golden recipe | `bins/delonix-runtime-bin/src/cmd/vmimage.rs` | `VmImageCmd::Build`, `VmImageStore`, `VmImage`, `customize_args`, `tool_failure_hint` |
| Declarative build | `bins/delonix-runtime-bin/src/cmd/vm.rs` | `VmBuildSpec` (`spec.build` of `kind: VirtualMachine`) |

### Scaffold

```bash
delonix vm init --vmfile [DIR] [--name <n>]   # or: delonix image vm init <name> [-d DIR]
```

Both write `VMfile` and `cloud-init/user-data.yaml` (*run* in a scratch directory). The scaffold is
meant to be a working recipe, and the `parseia_o_scaffold_que_escrevemos` test keeps it parseable.
Two things to correct by hand in the "Next:" hint it prints: the flag is `vm create --disk`, not
`--disk-image`; and see the `CLOUDINIT` note below about the file name.

### Building

```bash
delonix image vm build -t web:1.0 [-f VMfile] [--network] [--no-compress] [CONTEXT]
```

There is no `delonix vm build`; the build lives under `image vm`. Without `-f`, a `VMfile` in the
context is picked up automatically (like a `Delonixfile` beating a `Dockerfile`); with no `VMfile`,
the same command runs the built-in golden recipe instead (see [09](09-microvm-setup.md)). The
golden-recipe flags (`--k8s-version`, `--extra-package`, `--extra-run`, `--offline`, `--no-k8s`,
`--cri-bin`, `--delonix-bin`) are **refused** with a VMfile, and `--network` is refused without one.

### Instructions

`parse` is fail-closed: an unknown instruction is an error naming the supported set. Only `FROM`
may come first. `\` continuations are joined; a `#` starts a comment only at the start of a line
(so `RUN sed 's/#x/y/'` is not mangled).

| Instruction | Scope | Effect (`stage_ops` → `virt-customize`) |
|---|---|---|
| `FROM <ref> [AS <name>]` | opens a stage | See *What `FROM` accepts*. `as` is case-insensitive. |
| `RUN <shell>` | step | `--run-command` inside the guest; **offline** unless `--network`. |
| `COPY <src> <dst>` | step | Copy-in from the build context; `src` confined to the context (`build::safe_join`). Exactly two arguments. |
| `COPY --from=<stage> <src> <dst>` | step | `virt-copy-out` from that stage's disk to a host staging dir, then copy-in. The stage must be **named and declared earlier** — checked at parse time. |
| `ENV KEY=value` | step | Appends `KEY=value` to `/etc/environment` (a VM has no image config). One pair per line; the key and value are passed to a shell **unquoted**, so avoid spaces and shell metacharacters. |
| `USER <name>` | step | `useradd -m -s /bin/bash <name>` if the account does not exist. |
| `PASSWORD <user>:<password>` | step | Sets that account's password. It is baked into every copy of the image. |
| `ROOTPASSWORD <password>` | step | Sets root's password. Same caveat. |
| `SSHKEY <user> <path-or-key>` | step | Appends to `/home/<user>/.ssh/authorized_keys` (`~/` is expanded). The value must be a readable file or a key starting with `ssh-`/`ecdsa-`. The account must exist (create it with `USER` first). |
| `CLOUDINIT <path>` | step | Copies that file (context-confined) into `/etc/cloud/cloud.cfg.d/`, **keeping its file name**. cloud-init only loads files ending in `.cfg` from that directory, so name it e.g. `99-myimage.cfg` — the scaffold's `user-data.yaml` name is not picked up as written (not executed in this review; from the code path and cloud-init's documented behaviour). `vm create` still adds its own per-instance NoCloud seed on top. |
| `SIZE <n>G` | stage property | `qemu-img resize` **before** any step runs — growing after a `RUN` filled the disk would be too late, which is why it is not a step. |
| `HOSTNAME <name>` | stage property | Writes `/etc/hostname` (first op of the stage). |
| `VCPUS <n>` | image | Recorded as `default_vcpus`. |
| `MEMORY <n>` | image | Recorded as `default_memory`. |
| `HYPERVISOR <backend>` | image | Validated and canonicalized by `delonix_vm::valid_backend_name` (`ch` → `cloud-hypervisor`); recorded as `default_backend`. |
| `LABEL k=v` | image | **Parsed but not recorded** in the image metadata in this tree. |

There is no `CMD`/`ENTRYPOINT`: a VM boots an init. `CLOUDINIT` is the closest equivalent.

### What `FROM` accepts

`classify_base` is pure and decides the kind of reference:

1. **A named earlier stage** — wins over everything (`resolve_base`).
2. **`ubuntu:<rel>`, `debian:<rel>`, `rocky:<rel>`, `fedora:<rel>`** — first the project's own
   base image for that distro/release (local copy, then the official registry —
   `vmimage::official_distro_base`); if there is none, the distro's own cloud image, downloaded and
   verified against the publisher's checksum file (`vmimage::download_base`).
3. **`http://` / `https://` URL** — downloaded; verified against `<url>.sha256` when the publisher
   offers one, otherwise trusted on TLS alone and the build says so.
4. **Anything else** — a VM image already in the local store (`delonix image vm ls`). Any other
   `name:tag` is treated as a local tag, not as an unknown distro.

### What a stage is, and where the result lands

A stage is a **whole disk**, not a layer (`build`):

1. The base is **flattened** into `<work>/<stage>.qcow2` with `qemu-img convert` (no backing file,
   so the artefact never depends on the base still being present).
2. `SIZE` is applied, then all steps run in one `virt-customize` call (`--no-network` unless
   `--network`). `vmimage::customize_args` always appends an SELinux relabel as the last command
   (and turns off libguestfs's own deferred relabel), so SELinux guests do not boot with unlabeled
   files.
3. Named stages are kept for `COPY --from`; unnamed ones are not addressable. Only the **last**
   stage becomes an image.
4. Unless `--no-compress`: `virt-sparsify --in-place` (best effort), then
   `qemu-img convert -c -o compression_type=zstd`. zstd because the image becomes the read-only
   backing file of every VM created from it, so decompression speed is a runtime property.
5. The qcow2 is moved to `VmImageStore::qcow2_path(tag)` under `<DELONIX_ROOT>/vm-images/`, and a
   `VmImage` metadata record is saved next to it.

Recorded metadata: `digest` and `size`; `default_vcpus`/`default_memory`/`default_backend` from the
file; `cloud_init: true`; `built_by: "delonix <version>"`; `distro` and `kernel_version`
**inherited** only when the final `FROM` is a local image (a URL gives nothing to inherit, and the
code refuses to guess).

`vm create` applies the recorded defaults only where the caller did not decide: `--vcpus`/`--memory`
flags win over `VCPUS`/`MEMORY`; for the backend, `--backend` > the image's `HYPERVISOR` >
`DELONIX_VM_BACKEND` > `vm default-backend` > auto-detection (`resolve_vm_defaults` in `cmd/vm.rs`,
and `delonix_vm::create_with`).

**Offline by default, and why.** A `RUN` that reaches the internet produces a different image
depending on when it ran. `--network` is opt-in because the most common thing a VMfile wants is to
install a package. With `--network`, libguestfs's appliance gets its network from `passt`, which has
host traps documented in [09](09-microvm-setup.md#troubleshooting).

**Scratch space.** The work directory is created under `std::env::temp_dir()`
(`delonix-vmfile-<pid>`), i.e. `$TMPDIR` or `/tmp`, and holds one full flattened disk per stage. Point
`TMPDIR` at a filesystem with room — `/tmp` is often a small tmpfs and is emptied on reboot. A build
that fails leaves that directory behind (observed during this review); remove it by hand.

### Declarative build

`kind: VirtualMachine` can build its own disk with `spec.build` (`VmBuildSpec`): `context` (relative
to the **manifest's** directory), `file` (default `<context>/VMfile`), `tag` (default
`<metadata.name>:latest`), `compress`, `network`. `apply` calls the same `vmfile::build`. `disk` and
`build` are mutually exclusive.

### Worked example (parse-checked)

```dockerfile
FROM my-base:1.0 AS builder
RUN make -C /src

FROM my-base:1.0
SIZE 20G
HOSTNAME web
COPY --from=builder /src/app /usr/local/bin/app
ENV APP_ENV=production
USER app
VCPUS 2
MEMORY 2G
HYPERVISOR ch
LABEL org.opencontainers.image.title=web
```

With no image called `my-base:1.0` in the scratch store, the file parses and the build stops at
base resolution — before any disk work:

```text
$ delonix image vm build -t web:1.0 .
[1/2] builder: FROM my-base:1.0
error invalid argument: FROM my-base:1.0: no such local VM image, and it is not a URL nor a known cloud image (ubuntu:/debian:/rocky:) — see `delonix vm ls`
```

And the parser's refusals:

```text
VMfile:2: unknown instruction 'CMD' — supported: FROM RUN COPY ENV USER PASSWORD ROOTPASSWORD CLOUDINIT SSHKEY SIZE HOSTNAME VCPUS MEMORY HYPERVISOR LABEL
VMfile:2: no earlier stage named 'nope'
VMfile:2: invalid argument: unknown VM backend: 'vmware' (use 'cloud-hypervisor', 'libvirt')
--offline belong to the built-in golden recipe and mean nothing with a VMfile — the VMfile describes all of that itself
```

A full build (`virt-customize`, downloads, compression) was **not executed in this review**.

---

## Comparison

| | Dockerfile (Docker/BuildKit) | Delonixfile (`delonix build`) | VMfile (`delonix image vm build`) |
|---|---|---|---|
| Output | OCI image | OCI image (pushable, pullable by Docker) | bootable qcow2 + `VmImage` metadata |
| Unit of a stage | filesystem layers | a working container + rootfs | a whole flattened disk |
| `FROM` | image | image or earlier stage | distro:release, URL, local VM image, or earlier stage |
| `RUN` executes in | build container | working container via `exec` (rootless userns) | guest, via `virt-customize` (offline by default) |
| `COPY --from` | yes | yes (name or index) | yes (named, earlier stages only; `virt-copy-out`) |
| `COPY` flags `--chown/--chmod` | yes | no | no |
| `ADD` URL / archive extraction | yes | no (`ADD` = `COPY`) | no `ADD` |
| `RUN --mount` | secret, ssh, cache, bind, tmpfs | `type=secret` only | none |
| `--target` | yes | yes | no |
| `--platform` | yes | `linux/<arch>`, host binfmt required | no (images stay amd64 — [ADR-0018](../adr/0018-vm-images-stay-amd64.md)) |
| Layer cache | yes | rootless only, no GC | none |
| `CMD`/`ENTRYPOINT`/`USER`/`ENV` | image config | image config | no `CMD`; `USER` creates an account; `ENV` → `/etc/environment` |
| Resource hints | — | `CPUS`/`MEMORY`/`SECURITY` recorded, not applied | `VCPUS`/`MEMORY`/`HYPERVISOR` applied by `vm create` as defaults |
| Vulnerability gate | — | `SCAN` parsed, not enforced (use `image scan --fail-on`) | — |
| Accounts / keys / passwords | — | — | `USER`, `SSHKEY`, `PASSWORD`, `ROOTPASSWORD` |
| Disk size | — | — | `SIZE` (before any step) |
| Unknown instruction | error | error | error |

## If you change a grammar

- Keep both parsers **fail-closed**: an unknown instruction is an error, never a skipped line.
- A new instruction lands with a unit test in the parser's `mod tests`, a row in this page, and — if
  the scaffold uses it — the scaffold test still passing.
- If an instruction is parsed but not yet wired to an effect (as `SCAN`, `CPUS`/`MEMORY`/`SECURITY`
  and VMfile `LABEL` are today), say so here; a field the user writes and the engine ignores must be
  documented as such or removed.
- The Delonixfile parser lives in a library crate (`delonix-oci`), so it must not print; the
  VMfile parser is in the CLI binary. See [06 — Crates](06-crates.md).
