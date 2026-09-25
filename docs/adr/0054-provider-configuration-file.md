# ADR-0054: Providers are configured in one file per node, and a caller never has to name one

- **Status:** Proposed
- **Date:** 2026-09-26
- **Deciders:** Walter Angolar
- **Relates to:** ADR-0008 (the backend registry and why a remote target is not a manifest
  field), ADR-0044 D4 (the registry moves to `delonix-compute` with the port — this ADR does
  not move it), ADR-0049 (the Proxmox transport), ADR-0050 (`delonix provider ls|describe`),
  ADR-0041/0042 (the node contract), ADR-0010 (the management API stays local)

## Context

Three questions have to have ONE answer on a node: *which providers exist here*, *how does
the engine reach each one*, and *which one serves a request that names none*. Measured on
`origin/main` `4e9e23d0` (v4.4.0), each is answered today by a different mechanism, and they
disagree.

1. **A remote target exists only in the environment of the process that reads it.** The
   Proxmox target is `DELONIX_PROXMOX_URL`/`_NODE`/`_TOKEN_ID`/`_TOKEN[_FILE]`/`_SECRET`/
   `_INSECURE_TLS`/`_CA_FILE`/`_BRIDGE`/`_VLAN`, read at startup by
   `bins/delonix-runtime-bin/src/cmd/vmbackends.rs`. An interactive shell, a systemd unit
   running the node API, a timer and a CI job are four processes with four environments. The
   same node answers «is there a Proxmox provider?» differently depending on who asks.

2. **The parser is written twice.** `cmd/network_zone_providers.rs` carries its own copy of
   `proxmox_target_with`/`proxmox_auth`/`credential_value` (the only difference today is one
   comment). Two parsers of one configuration are two answers waiting to diverge — the
   situation `KindFacts` and `fw_rule_tail` were built to end elsewhere in this repo.

3. **The persisted default silently loses to the environment.** `vm default-backend --set`
   writes `<DELONIX_ROOT>/vm-default-backend`, and `get_default_backend` validates the name
   against the registry **of the process that reads it** (`canonical_backend_name` →
   `with_backends`). A default of `proxmox` set from a shell with the variables, read by a
   process without them, returns `None` — and `create_with` falls through to auto-detection
   and creates the VM **locally**, without a word. That is a VM on the wrong hypervisor
   reported as a success. (Read from the code; not yet reproduced live — the reproduction is
   the first check of the implementation slice.)

4. **The default is not the one the installer should give.** `auto_detect` walks the
   registry in order, and `cloud-hypervisor` is first; `install.sh` installs Cloud Hypervisor
   on x86-64 where it can. So a fresh install defaults to Cloud Hypervisor, not libvirt, and
   nothing on the node says so — the choice is an accident of registration order.

The consequence for anyone driving the engine through the CLI or the node contract: to get a
predictable provider they must name it on every call, which means they must know it. A caller
that does not want to know — a script, an automation, a control plane — cannot rely on the
node having decided.

## Decision

### D1. One file declares the node's providers

```yaml
# /etc/delonix/providers.yaml   (or $XDG_CONFIG_HOME/delonix/providers.yaml)
apiVersion: config.delonix.io/v1
defaultProvider: libvirt            # what serves a request that names no provider

providers:
  - type: libvirt                   # local: nothing to configure; listed = enabled
    uri: qemu:///system             # optional; default is the engine's own choice

  - type: proxmox
    url: https://pve.example:8006   # the API endpoint the vendor documents
    node: pve                       # ONE node, as today (ADR-0008) — never picked for you
    auth:
      tokenId: delonix@pve!engine   # the vendor-recommended form: an API token
      tokenSecretFile: /etc/delonix/proxmox.token   # 0600, owner-only, or:
      # secretRef: proxmox-engine   # a `kind: Secret` with tokenId + tokenSecret
    tls:
      caFile: /etc/delonix/pve-ca.pem   # verify an internal CA instead of switching off
      # insecureSkipVerify: true        # opt-in, never a fallback after a TLS error
    network:
      bridge: vmbr0
      vlan: 20
```

- **The format is YAML** because `serde_yaml` is already in the tree and every other document
  an operator writes for this engine is YAML. It carries an `apiVersion` so the file can
  change without guessing, and it is **not a Kind**: it is not applied by `stack apply`, not
  owned by a stack, and has no `metadata` — it configures the node the Kinds run on.
- **No secret value is ever inline.** `tokenSecret:`/`password:` with a value is refused at
  parse time, naming the two accepted forms. A file of references can be `0644`, reviewed and
  committed to an inventory; a file with a token cannot. `tokenSecretFile` is refused unless
  only its owner can read it (the rule `credential_value` already applies to `_FILE`).
- **Password auth stays accepted, and says so** (`auth: {username, passwordFile}`): a freshly
  installed node has an account before it has a token. `provider describe` marks it as the
  weaker form, because the vendor recommends tokens (revocable without touching an account,
  privilege-separated, no ticket to expire).
- **One entry per `type` in this version.** `name:` is reserved (defaults to the type) so two
  Proxmox targets can come later without a format change; a second entry of the same type is
  refused today, naming the phase that lifts it. The registry id, the record's `backend`
  field and `--backend` keep using the type name, so nothing already recorded changes.

### D2. Where the file is found, and why the first one found wins

1. `DELONIX_PROVIDERS_CONFIG=<path>` — explicit, like `DOCKER_CONFIG`.
2. `$XDG_CONFIG_HOME/delonix/providers.yaml` (default `~/.config/…`) — a rootless user.
3. `/etc/delonix/providers.yaml` — the node, written by whoever provisions it.

**The first file found is the configuration. Files are never merged.** Two files that each
contribute half a provider are how an operator reads one and the engine obeys the other. When
a lower-precedence file exists and is being ignored, `provider ls` says so on one line.

### D3. Which provider serves a request

```
explicit request (--backend, spec.backend)
  > the provider already on the record (an existing VM never moves)
  > DELONIX_VM_BACKEND
  > defaultProvider in the file
  > <root>/vm-default-backend   (read with a deprecation warning, removed later)
  > auto-detection               (only when NO file exists)
```

The rule that fixes Context §3: **a `defaultProvider` the node cannot serve is an error, never
a fall-through.** If the file names `proxmox` and the Proxmox entry is missing, invalid, or its
credential unreadable, `vm create` without `--backend` fails with the reason and the file path
(exit 69, `NodeUnavailable`-class for an unreachable node; `Invalid` for a bad file). The node
said what it wants; creating the VM elsewhere is not a degraded success, it is the wrong
outcome.

Auto-detection survives only for a node with **no file at all** — a developer who never ran
the installer keeps today's behaviour byte for byte.

### D4. The environment stays, as an override that says it is one

`DELONIX_PROXMOX_*` keeps working (CI hands a credential over this way), and when
`DELONIX_PROXMOX_URL` is set it **replaces** the file's `proxmox` entry as a whole — never
field by field, for the same reason files are not merged. `provider describe proxmox` prints
the source of every value (`file:/etc/delonix/providers.yaml` or `env`), so an override is
visible and not a mystery.

Both parsers of Context §2 become one: `ProviderConfig::load()` in the `-bin` composition
root, whose result feeds `vmbackends.rs` and `network_zone_providers.rs`. Registration is
unchanged in shape (ADR-0008: a factory, no I/O until selected), and where the registry lives
is ADR-0044 D4's business, not this ADR's.

### D5. The CLI and the node contract read the same file, so a caller need not know

Every surface of the engine — the CLI, the node contract (`delonix-node-api`), the MCP server
— is a process on the same node that calls `ProviderConfig::load()` with the same precedence.
There is no second place a provider can be configured, so they cannot disagree.

For the node contract this means: **the provider field of a VM request is optional.** Empty
means «the node decides» (D3); the response and the stored record carry the provider that
actually served it, so a caller can display or audit it without ever choosing it. A caller
that does need a specific provider still names it and is refused by name if the node cannot
serve it (ADR-0050 D6). This is a capability of the engine phrased for any client: a node is
told once which provider backs it, and every request inherits that.

### D6. `install.sh` writes the default file for a development node

The installer writes `providers.yaml` with `defaultProvider: libvirt` and a `libvirt` entry
(system path for a root install, XDG path for `--user`) **only if no file exists** — it never
rewrites an operator's file. `--vm-provider <libvirt|cloud-hypervisor>` changes the default
it writes; `--no-vm` writes nothing. This replaces the accident of Context §4 with a stated
choice, and a node whose installer ran is never on the auto-detection path.

A production node is configured by whoever provisions it, by writing the same file and the
token file it references. Nothing in the engine distinguishes the two cases: a development
node and a production node differ in what their file says, not in how it is read.

### D7. The CLI surface

- `delonix provider config show` — the effective configuration, secrets redacted, each value
  with its source, and the file(s) ignored by precedence.
- `delonix provider config validate [-f <path>]` — parses and checks without registering or
  contacting anything; exit 0/1 for use before a file is installed.
- `vm default-backend` becomes a thin reader of `defaultProvider` and its `--set` writes the
  file; its old state file is migrated once and removed.

`provider describe <p> --probe` (ADR-0050) stays the only command that contacts a remote node.

## Consequences

- A node answers «which provider» the same way to every process; the silent local fallback of
  Context §3 is gone.
- The duplicate Proxmox parser is gone.
- A development install is predictable (libvirt) and says so in a file anyone can read.
- **Cost:** a new configuration surface with a published format — `config.delonix.io/v1` is a
  promise for the rest of `0.x` of the file, and `docs/cli-stability.md` has to list it.
- **Cost:** a `defaultProvider` that cannot be served now fails a `vm create` that used to
  succeed locally. That is the point, and the release notes have to say it in those words.
- **Not changed:** the record's `backend` field, the registry's four rules (ADR-0008), the
  fact that one Proxmox entry addresses one node, ADR-0010 (nothing here is remote).

## Implementation slices

1. Reproduce Context §3 live (shell sets the default with the variables, a process without
   them creates locally) and keep it as a check in `scripts/e2e.sh` that fails before slice 2.
2. `ProviderConfig` (parse, precedence, refusal of inline secrets, one-per-type), the single
   loader feeding both registration sites, D3's fail-closed default. Tests with a map/tempdir,
   never the process environment.
3. `provider config show|validate`, `vm default-backend` on the file, `pt.po` entries, the
   schema of the file published next to the manifest schema.
4. `install.sh` (D6) and its idempotence check (a second run leaves an edited file byte-equal).
5. The node contract's optional provider field (D5), in `delonix-node-api` once it lands.
6. `provider-lifecycle.sh` runs each provider **from the file alone**, with no `--backend`
   and no `DELONIX_PROXMOX_*` in the environment — the proof that a caller need not know.

Slices 2–3 touch the same registration code as the in-flight ADR-0044 P4b.2 work; they start
after it merges, not beside it.
