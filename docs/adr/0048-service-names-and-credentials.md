# ADR-0048: Standard service names, the SVC column, `hosts sync` and service credentials

## Status

Proposed 2026-09-20. Scope decided with the owner the same day (table below); nothing is built.

## Context

A user who starts a web service today has to invent the name to reach it, and the engine says
nothing to help:

1. **Three spellings, none suggested.** The internal DNS answers `<name>`, `<name>.delonix.io`
   (legacy) and `<name>.<ns>.delonix.internal`. `container ls`, `vm ls` and `stack ls` print none
   of them, so the name that works is found by reading the docs.
2. **A name is not a service.** `kind: Service` (ADR-0032) publishes a selected set under
   `<name>.<ns>.delonix.internal`, and a bare workload answers on the same suffix, so a client
   cannot tell a load-balanced set from one container by its name.
3. **The listing does not say what to open.** `container ls` shows `PORTS` as `host:container`.
   Whether it is HTTP or HTTPS, and what URL a browser takes, is not said. A container exposed with
   `--expose` is served by the L7 proxy on `:8080` under its FQDN, and nothing tells the user.
4. **Credentials live in READMEs.** The first thing anyone does with Grafana, Zabbix or an
   appliance is look up the login. The engine holds it (a `kind: Secret`, an env var, or the
   vendor default) and never shows it.

## Decisions taken with the owner (2026-09-20)

| Question | Choice |
|---|---|
| Standard name | **`<name>.<ns>.svc.delonix.internal`**, like the k8s `<svc>.<ns>.svc`. The existing `<name>.<ns>.delonix.internal` keeps resolving as an alias |
| Credentials | **Declared**: a `credentials:` field referencing a `kind: Secret`, plus an embedded catalogue of vendor defaults for known images. Never guessed — no source prints `-` |
| `/etc/hosts` | Names and URLs are **shown** by the listings; **`delonix hosts sync`** is the one opt-in command that writes the block (or prints it). Rootless it works without root through the holder DNS and `curl --resolve` |
| Order | This ADR first, then three phases |

## Decision

### D1 — The service name

The name a user is told to use is `<name>.<ns>.svc.delonix.internal`, for every workload that
serves HTTP(S): container, pod member group, VM, `kind: Service`. One spelling across kinds is the
point; it is what the listings print and what `hosts sync` publishes.

`parse_internal_name` (`delonix-sdn::infra`) strips `.delonix.internal` and splits the last label
as the namespace. With `web.data.svc.delonix.internal` that reads `("web.data", ns="svc")` — wrong.
The new suffix is therefore matched **first**, and only when **two labels** precede `.svc`
(`<name>.<ns>`). `x.svc.delonix.internal` keeps its old meaning (namespace `svc`), so an existing
namespace called `svc` is not broken. `<name>.<ns>.delonix.internal`, `<name>.delonix.io` and the
bare `<name>` stay as aliases; nothing is removed.

A service name is **derived**, never stored: `name`, `namespace` and the suffix are all known, so
there is no second record to drift from the workload.

### D2 — The service view

One pure function builds the rows every listing shares, so `container ls`, `vm ls`, `stack ls`,
`describe` and `hosts sync` cannot disagree (the `fw_rule_tail` rule: the writer and the reader
share one definition). A row is `{ fqdn, scheme, port, source }`, from the places a workload is
already published as HTTP:

| Source | Row |
|---|---|
| `container run --expose <port>` | `http`, listener `:8080` (the auto-route port) |
| `kind: HTTPRoute` / `Ingress` backend | one row per host of the route, `https` when its entrypoint terminates TLS, else `http`, on the entrypoint port |
| `VirtualMachine.spec.expose` (ADR-0046) | as the route it lowers to |
| `kind: Service` | its own name, port from `spec.port` |

Scheme is `https` only when TLS is terminated on that entrypoint. It is never inferred from a port
number (`8443` is not proof of TLS).

Listings gain a **`SVC`** column with `scheme://fqdn:port` (several rows join with `, `), hidden
when no row exists on the whole listing, the way `NAMESPACE` already hides. `describe` prints the
full list. `--output json` carries `services: [{fqdn, scheme, port, url}]`.

### D3 — `delonix hosts sync`

A workload on the SDN is not reachable from the host, only through the L7 proxy published on
`127.0.0.1`. So the URL a browser can open is `scheme://<fqdn>:<listener port>` with the name
pointed at `127.0.0.1` in the host's `/etc/hosts`. That is exactly the `hosts: [host]` mechanism of
ADR-0046 (per-state-root block, refusal on foreign entries, `DELONIX_HOSTS_FILE`).

- `delonix hosts sync` turns the service names of every running workload into that block, and
  records `on` so later starts and stops keep it current. `--print` shows the block without
  writing; `--off` removes it and stops maintaining it.
- Without root the write is refused with the block printed (the ADR-0046 behaviour), never a silent
  no-op. The names still work **inside the SDN** through the holder DNS, and from the host with
  `curl --resolve <fqdn>:<port>:127.0.0.1 …`, which the listings print.
- It is a state, not a default: nothing writes `/etc/hosts` until the user ran the command once.

### D4 — Credentials

`credentials:` is accepted on `Container`, `Pod`, `VirtualMachine` and `HTTPRoute`:

```yaml
credentials:
  user: admin
  password: { secret: grafana-admin, key: password }   # a kind: Secret, never the value
  note: "created at first login"                       # optional, free text
```

The record keeps the **reference**, never the value. Listings and `describe` take
`--credentials`: it adds `USER` and `PASSWORD`, and the password prints as `(secret grafana-admin)`
unless `--reveal` is also given — the `secret inspect --reveal` precedent. Without `--reveal` a
copied listing does not leak a password.

For images with a well-known vendor default (Grafana, Zabbix, Prometheus, Loki, and the appliance
images this repo builds) an embedded catalogue supplies `{user, default password | none, env
override}`. Rules, so it cannot mislead:

1. A declared `credentials:` always wins.
2. A catalogue default is printed with `(vendor default — change it)`; a service whose catalogue
   entry names an env var (`GF_SECURITY_ADMIN_PASSWORD`) that the container overrides prints
   `(changed — see the env)` instead of the default, which would now be wrong.
3. No source → `-`. The engine never invents a login.

The catalogue is a table of public images and their documented defaults. It names no platform and
no consumer.

## Phases

1. **Names and the SVC column.** D1 and D2: `parse_internal_name`, the shared view, the column in
   `container/vm/stack ls`, `describe`, `--output json`, the DNS answering the new suffix.
2. **`hosts sync`.** D3.
3. **Credentials.** D4: the field, the reference, `--credentials`/`--reveal`, the catalogue.

Each phase is a PR with a live proof: phase 1 resolves the new name from a container inside the SDN
and prints it in the listing; phase 2 writes the block into a test hosts file and refuses without
root; phase 3 shows a declared login, a catalogue default, and a changed one.

## Consequences

- One name per service to teach and to type; the listings say it.
- `stack ls` and `container ls` gain a column, hidden when empty, so scripts that cut columns by
  position are not broken on hosts that expose nothing.
- Credentials become a field in the manifest schema, so the schema and docs move with phase 3.
- Aliases stay forever unless a later ADR removes them.

## Rejected

- **`<name>.delonix` short names.** No namespace in view; tenants would collide.
- **Writing `/etc/hosts` by default.** Fails without root and breaks ADR-0046's "each target opt-in".
- **A catalogue as the only source.** It prints the factory default after the user changed it.
- **Storing the password in the record.** The vault exists so the record does not hold it.

## Open questions

1. Should a workload that exposes several ports get one name per port (`web-admin.<ns>.svc…`) or
   one name and several URLs? The design above uses one name and several rows.
2. Does the pod name resolve to the group, or each member? Members are `<pod>-<name>`; the group
   name is the natural service, as in k8s.
3. Should `stack ls` show the SVC for the whole stack in `describe` only, to keep the table narrow?
