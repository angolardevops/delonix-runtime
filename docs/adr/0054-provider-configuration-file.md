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
   reported as a success. **Reproduced live** (binary `4abc81353`, isolated root, nothing
   contacted — the target is `https://pve.invalid:8006`): `vm default-backend --set proxmox`
   with the variables writes `proxmox`; read back with them it answers `proxmox`, read back
   without them it answers `none (auto-detection…)` with **rc=0 and no warning**; and
   `vm create` without them takes the LOCAL path (`DX-4000 no such VM image`) where with them
   it goes to the node (`DX-9510 … pve.invalid`). The same run shows Context §2 from the
   outside: the token warning is printed **twice**, once per parser.

4. **The default is not one the node states.** With no `--backend`, `create_with` prefers
   libvirt for a cloud image (no kernel) when libvirt is installed, and otherwise walks the
   registry in order, where `cloud-hypervisor` is first — so a direct-kernel VM on a host
   with both lands on Cloud Hypervisor. Each rule is reasonable; none is written anywhere an
   operator reads, and a control plane cannot learn from the node which one applies.
   *(Corrected 2026-09-26: the first version of this paragraph said a fresh install
   defaults to Cloud Hypervisor, which the code does not do for a cloud image.)*

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
  - type: libvirt                   # local: nothing to configure; listed = declared

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

1. **Done.** Context §3 reproduced live, and kept in `scripts/e2e.sh` (section «vm (só o que
   não precisa de hipervisor)») as two `xfail ADR-0054` checks with a control beside them,
   in a state root of their own so the default they write never reaches another section.
   Verified in both directions: 2 XFAIL against this binary, and 2 XPASS — which fails the
   gate until the mark is removed — when the conditions are met.
2. **Done.** `cmd/providers_config.rs` in the composition root: parse (`deny_unknown_fields`,
   `apiVersion` checked, one entry per type, `name` reserved, an inline `tokenSecret`/
   `password` refused by name), D2's location order with the first file winning, and the
   Proxmox entry translated into the `DELONIX_PROXMOX_*` keys so the ONE existing parser
   (`vmbackends::proxmox_target_with`) reads both sources — `network_zone_providers` now
   calls it instead of its own copy, which is deleted. D4: `DELONIX_PROXMOX_URL` in the
   environment replaces the file's entry as a whole. D3 in the engine:
   `set_configured_default_backend` (the engine never opens the file), a default the
   process cannot serve returned as written instead of dropped, so selecting it fails in
   the unavailable class (69) naming the provider; an unreadable file fails every choice
   that would have used it. The two slice-1 `xfail`s are ordinary checks now, and the
   battery gained the file cases (default + target from the file alone, a default with
   no entry, an inline secret, a missing explicit path, and a bad file not stopping
   `container ls`). `uri:` on a libvirt entry is refused as unknown in this version.
   Other binaries (`delonix-mgmt`, `delonix-mcp`, `delonix-cri`) do not read the file yet —
   that is slice 5, with the node contract.
3. **Done.** `provider config show` says which file is read (and every existing file the
   D2 order skips — `provider ls` says it too), the default provider and where it comes
   from (environment, file, the legacy per-root default, auto-detection), and each provider
   with its settings; a credential is shown only by where it comes from — the battery greps
   both outputs for the token's value. `provider config validate [-f]` runs `parse` plus
   what the file points at, through the registration's own reader, contacting nothing (exit
   1 on a default with no entry, an inline secret, a token file others can read).
   `vm default-backend --set/--clear` edits the `defaultProvider:` line of the file this
   process reads (or creates the user/system file the way `install.sh` does), keeps every
   other line and the file's mode, refuses to write a result this build would reject, and
   MOVES the legacy per-root default instead of leaving a second answer behind.
   `provider config schema` prints the JSON Schema generated from the parser's own types,
   published as `docs/schema/v1/providers.json` with a test that fails when the two differ,
   and `docs/cli-stability.md` lists the format.
4. **Done.** `install.sh --vm-provider <libvirt|cloud-hypervisor>` (default `libvirt`) writes
   `/etc/delonix/providers.yaml` (`~/.config/delonix/` with `--user`) inside the VM block, so
   `--no-vm` writes nothing, and ONLY when the file is absent — `set -C` refuses an existing
   path, symlink included, at the moment of writing. An unknown provider is refused right
   after argument parsing, before the host is touched. The installer's verification asks the
   INSTALLED binary for the default (`vm default-backend` against that file) and compares it
   with the file's `defaultProvider`; a binary older than this ADR is warned about, not
   failed. `scripts/test_install_providers.py` extracts the function from the script itself:
   a second run leaves an edited file byte-equal, a symlink is neither followed nor replaced
   (3 of its 7 tests fail with the guard removed).
5. The node contract's optional provider field (D5), in `delonix-node-api` once it lands.
6. `provider-lifecycle.sh` runs each provider **from the file alone**, with no `--backend`
   and no `DELONIX_PROXMOX_*` in the environment — the proof that a caller need not know.

Slices 2–3 touch the same registration code as the in-flight ADR-0044 P4b.2 work; they start
after it merges, not beside it.
