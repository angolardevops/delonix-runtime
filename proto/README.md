# Delonix node contract (`delonix.node.v1`)

**Status: draft — ADR-0040.** Nothing serves or consumes these files yet.

The engine's local contract with **any** client on the node — a script, Ansible or
Terraform over SSH, Prometheus, an operator's tool, another program. The engine names
no consumer and is shaped around none: whoever consumes it adapts to it, never the
other way round. Served — once phase P5 lands — by the `delonix-node-api` binary on a
**local unix socket**, as gRPC and as HTTP/JSON generated from the same files. It is
not a remote API (ADR-0010): remote access, identity and tenancy are the business of
whatever sits in front of it.

Three rules a review enforces on every change:

1. No tenant, account, plan, quota or billing field. `namespace` is the engine's
   isolation namespace.
2. No provider name in a field the caller writes; provider knobs go in
   `ProviderExtensions`, and a provider rejects keys it does not know.
3. No consumer in the contract: no field, comment or RPC exists "for" a particular
   client. A requirement that came from one consumer is written as the capability it
   is, in the engine's own vocabulary (its Kinds and resources).

## Encodings

- **gRPC** — the services as written.
- **HTTP/JSON** — from the `google.api.http` option on each RPC, resource-oriented in the
  engine's own vocabulary:
  - namespaced resources: `/v1/namespaces/{namespace}/{containers|pods|virtualmachines|networks|volumes}[/{name}]`;
  - actions as custom verbs: `…/{name}:start`, `:stop`, `:connect`; updates are `PATCH`
    with an `update_mask`;
  - node and operations: `/v1/node`, `/v1/node/health`, `/v1/providers`, `/v1/operations/{id}`;
    watches and logs are server-streamed `GET`s;
  - images are addressed by QUERY (`/v1/images:get?reference=…`), never by path: a
    reference such as `alpine:3.20` contains `:`, which a path would read as a verb;
  - `Exec` and `Console` stream from both sides and have no HTTP mapping: REST serves
    them over WebSocket (ADR-0040 D4).
- **OpenAPI 3** — `docs/api/openapi.yaml`, GENERATED from these files; never edited by hand.

Every RPC has its own `<Rpc>Request`; responses are the resource or an `Operation`
(the written exceptions in `buf.yaml`). The vendored `google/api` protos live in
`third_party/googleapis/` — see its README for the source commit.

## Check locally

Needs `protoc`, `buf` (v1.73.0) and `protoc-gen-openapi` (gnostic v0.7.1) — the CI job
`contract` pins the same versions:

```bash
go install github.com/bufbuild/buf/cmd/buf@v1.73.0
go install github.com/google/gnostic/cmd/protoc-gen-openapi@v0.7.1
python3 scripts/contract_gate.py            # format, lint, breaking, mapping, OpenAPI
python3 scripts/contract_gate.py --update   # regenerate docs/api/openapi.yaml
```
