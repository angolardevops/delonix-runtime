# ADR-0042: One engine API — one version number, Richardson maturity, published docs

- **Status:** Accepted (2026-09-17) — steps A and B delivered (#388, #389); step C started 2026-09-25: `delonix-node-api` exists on the socket and serves `NodeService.ListProviders` as gRPC and HTTP/JSON (ADR-0050 D5); since 2026-10-02 (slice C1) also `GetNodeInfo`, `GetHealth`, `GetCapacity` and `GET /openapi.json` (the committed generated document, embedded at build time). Since slice C2 also `/docs` (Swagger UI 5.33.1) and `/redoc` (ReDoc 2.5.4), from files embedded in the binary — byte-identical to the npm packages, in `third_party/node-api-docs/`, checked against its `SHA256SUMS` by `build.rs` — under a Content-Security-Policy that allows only the socket (measured in a browser: ReDoc's footer logo from `cdn.redoc.ly` is blocked before any request leaves). **Slice C3 (2026-10-03)** closes step C with the transcoding spike's result (see «Spike result: REST transcoding» below): the REST routes are generated at build time from the `google.api.http` annotations. **Step E started 2026-10-03 (slice E1, network reads)**: `NetworkService.GetNetwork` and `ListNetworks` are served on both encodings. A network belongs to the node: it is reported in `default`, and another namespace has none. `Realized` is read from the dataplane record; the implementation (driver, bridge, gateway) travels under `spec.extensions.by_provider["linux"]`. With a resource and a list on the socket, three step D conventions gained their first producer: `meta.etag` is the `ETag` header and a matching `If-None-Match` is `304`; `label_selector` (equality grammar; set-based forms refused); `page.page_size`/`page.page_token` with the next page as a `next` link, mirrored in the `Link` header. `Network` and `ListNetworksResponse` gained `links`. The four network mutations answer `UNIMPLEMENTED`: they return an `Operation`, which needs the persisted operation record first. **Slice E2 (2026-10-03, the operation record and the first mutations)**: `NetworkService.CreateNetwork` and `DeleteNetwork` are served, and `OperationService.GetOperation` and `ListOperations` with them — see «Slice E2: the operation record» below. `ConnectContainer`/`DisconnectContainer` (they answer a `Container`, which the container wave brings), `WatchOperation` and `CancelOperation` still answer `UNIMPLEMENTED`. **Step D started 2026-10-02 (slice D1)**: every REST error is an RFC 9457 `application/problem+json` document built by the engine's `codes::problem` (with `grpc_status`), and the published OpenAPI declares it — `scripts/contract_gate.py` applies one documented transform (`problem_json`) to the plugin's output, which only knows `google.rpc.Status`; gRPC keeps `google.rpc.Status`. New codes DX-4001 (no route), DX-6001 (not served yet, HTTP 501) and DX-9005 (internal). A method a path does not answer is 405 with `Allow` and a problem document. **Slice D2**: `Link` in the contract (`rel`, `href`, `method`), `GET /v1` (`GetApiRoot`: the API version and a link to every resource served, and only those), `links` on `NodeInfo`, `Health`, `Capacity` and `ListProvidersResponse` (a filtered list's `self` keeps the filter), and every JSON answer mirrors its `links` in an RFC 8288 `Link` header read back from the body. Still to come in D: `ETag`/`If-Match`, pagination, idempotency — they matter once mutations and lists are served (step E). `GET /v1` moved to step D: the entry point is made of `links`, which D puts in the contract, and serving it before would publish a shape the OpenAPI does not declare. **Transcoding spike result**: the proto3 JSON of every message is generated from the same files (`pbjson`, proto field names = the OpenAPI's `naming=proto`); the HTTP route of a `google.api.http` annotation is written by hand per RPC for now (one exists), and a generic transcoder over the annotations is the next slice of C. The `delonix-node-proto` crate is not split out: with one consumer it would be the dead scaffolding ADR-0040 P1 refuses, so the stubs live in `delonix_node_api::proto` as the CRI's do
- **Date:** 2026-09-17
- **Deciders:** Walter (owner)
- **Builds on, does not reopen:** ADR-0040 D4 (one contract in `proto/delonix/node/v1`, gRPC
  and REST from the same files, local unix socket, socket activation), ADR-0041 (coverage,
  promise tiers, lifetime), ADR-0010 (**Rejected** — the API stays local; stays rejected).
- **Discovery:** [`docs/discovery/55_API_UNICA.md`](../discovery/55_API_UNICA.md) — the
  measured map this ADR is decided from.

## Context

Nothing of this engine is in production, so the API can still be made one thing instead
of the several it is today. Measured on `origin/main` (2026-09-17):

- **Two engine APIs share the number `v1`.** `delonix-mgmt` serves 32 hand-written paths (40 operations)
  (`/v1/containers/:id/action`, `/v1/net/publish`, …) — RPC-shaped, no OpenAPI, no
  pagination, no idempotency, mutations answering the CLI's stdout. The node contract
  `delonix.node.v1` (`proto/`) is resource-oriented, generates `docs/api/openapi.yaml`
  under a CI gate, and **nothing serves it yet**.
- **The `v2` in this repository is not the engine's.** `/v2/<repo>/manifests` is the OCI
  registry protocol and `/api/v2.0` is TrueNAS's API — third-party protocols, unchanged.
  The one `v2` that is this repository's is `http_post_json`/`http_get_auth`/
  `http_post_stream` in `delonix-image`, documented as a transport to `/v2/cli` and
  `/v2/studio/designs` of a "platform" and a "Console": **a consumer's API inside the
  engine**, against «O que o motor não conhece: nenhum consumidor». They have **no caller
  in this repository** — only the re-export.
- The owner asked for: one version number; a server that brings up the engine's API for
  anyone integrating with it; OpenAPI docs in the style of FastAPI (interactive, complete);
  and the maturity levels of a RESTful API.

## Decision

### D1. One API, one number: `v1`

- **The engine has one API: the node contract `delonix.node.v1`**, served by
  `delonix-node-api` (ADR-0040 P5). Every path it serves starts with `/v1`. There is no
  `v2` of the engine until a breaking change needs one, and `buf breaking` (P1 gate) is what
  says a change is breaking.
- **Unversioned by convention, and only these:** `/openapi.json`, `/docs`, `/redoc` (D3, the
  paths FastAPI and every OpenAPI tool expect) and `/metrics` (the Prometheus scrape path).
  They describe or observe the API; they are not resources of it.
- **`delonix serve api` is the door** (ADR-0040 D2.4 as amended): it `exec`s the API
  server. The user only knows `delonix`.
- **`delonix-mgmt` is migrated route by route onto the contract and then removed.** Each of
  its 40 operations is mapped in the discovery document to an RPC, to a gap the contract must
  close, or to a refusal with a reason. No path is dropped silently.
- **The consumer transport leaves the engine.** `http_post_json`, `http_get_auth` and
  `http_post_stream` are removed from `delonix-image`: they serve one client's API, and no
  code here calls them. A client that needs an HTTP helper owns it.
- **Out of scope, said so:** the `apiVersion` of manifests (`compute.delonix.io/v1alpha1`)
  versions the **manifest schema**, not this API, and has its own promise in
  `cli-stability.md`. Aligning it is a separate decision.

### D2. Maturity: Richardson 0–3, plus the practices a client relies on

The four Richardson levels, each with what makes this contract meet it — and each checked
by the conformance suite (D4), not asserted:

| Level | Meaning | How the contract meets it |
|---|---|---|
| 0 | HTTP as a transport | JSON over HTTP/1.1 on the unix socket (gRPC alongside, same RPCs) |
| 1 | Resources | Every resource has one URI: `/v1/namespaces/{namespace}/{containers\|pods\|virtualmachines\|networks\|volumes}/{name}`, `/v1/images:get?reference=`, `/v1/operations/{id}`, `/v1/node` |
| 2 | HTTP verbs and status codes | `GET` safe and cacheable; `POST` on a collection creates → `201 Created` + `Location`; `PATCH` with `update_mask`; `DELETE` → `204`, or `202` + an `Operation`; long work → `202 Accepted` + `Location: /v1/operations/{id}`; actions are AIP-136 custom methods (`POST …/{name}:start`); `404`/`409`/`412`/`400`/`503`/`504` carry the engine's error classes (the same `DX_*` the CLI exits with) |
| 3 | Hypermedia (HATEOAS) | Every resource and `Operation` carries `links` — `self`, the collection, the actions valid **in its current state** (`start` only when stopped), `operation` while one runs — and the REST encoding mirrors them in RFC 8288 `Link` headers. A client navigates from `GET /v1` (the entry point, with links to every collection and to the docs) without building a URI by hand |

`links` is a field **in the contract** (`repeated Link links`), not something the REST
side invents: an extra JSON key the OpenAPI does not declare would make the published
schema lie. gRPC clients receive it too and may ignore it.

The practices a client depends on, beyond the levels:

- **Errors — RFC 9457 `application/problem+json`**: `type`, `title`, `status`, `detail`,
  `instance`, plus `code` (the `DX_*` class) and the gRPC status. One shape for every error.
- **Idempotency**: `request_id` on every mutation (ADR-0040 D4); the REST side also accepts
  `Idempotency-Key`. A replay returns the first answer.
- **Concurrency**: the resource `etag` is the `ETag` header; `If-Match` on `PATCH`/`DELETE`;
  a mismatch is `412 Precondition Failed`, never a silent overwrite.
- **Pagination**: `page_size` + `page_token` → `next_page_token`, mirrored as `Link: …;
  rel="next"`.
- **Filtering**: `label_selector` on every `List`.
- **Async**: an `Operation` is persisted before it is acknowledged, and a restart ends it
  `FAILED/Interrupted`, never `RUNNING` forever (ADR-0040 D4).

### D3. Docs in the style of FastAPI, served by the API itself

The server publishes, on the same socket:

- `GET /openapi.json` — the OpenAPI 3 document **generated from `proto/`** (the same one the
  P1 gate keeps committed), never written by hand;
- `GET /docs` — Swagger UI (try-it-out against the same socket);
- `GET /redoc` — ReDoc.

The UI assets are **embedded in the binary at a pinned version, with their checksums
verified at build time**: a node may be offline, and loading a script from a CDN into the
page that drives the engine's API is a supply-chain hole. Reaching the docs from a browser
is a port-forward or an SSH tunnel to the socket — the same path as the API (D5).

**"Complete" is a gate, not an intention.** `scripts/contract_gate.py` gains a completeness
check over the generated document, and fails on: an operation without `summary` and
`description`; a request or response body without a schema; a field without a
description; an operation without its error responses (`problem+json`); a mutation
without an example. Measured against today's `openapi.yaml` before the gate lands, so it
starts at a true baseline and ratchets down.

### D4. Proven by a conformance suite

A suite drives the **generated client** against a real `delonix-node-api` in an isolated
root, and checks each level of D2 as behaviour: the `Location` of a `201`, the `links` of a
stopped container offering `start` and not `stop`, `412` on a stale `If-Match`, the replay
of an `Idempotency-Key`, the `next` link of a page, an error body that validates as
`problem+json`. It joins the E2E battery.

### D5. Local, as ADR-0010 and ADR-0040 decide

The API listens on a unix socket with `SO_PEERCRED` (the calling uid only). No TCP, no TLS,
no token, no identity in the engine. Integrating from off the node means putting one's own
proxy or tunnel in front — which is where identity and TLS belong.

## Delivery

Each step is one PR, measured, with the E2E battery green.

| Step | What | Done when |
|---|---|---|
| A | This ADR and the discovery map | merged — #388 |
| B | Remove the consumer transport (`http_post_json`, `http_get_auth`, `http_post_stream`) | no caller, no re-export, battery green — #389 |
| C | `delonix-node-proto` (prost/tonic) + `delonix-node-api` skeleton on the socket: `GET /v1`, `/openapi.json`, `/docs`, `/redoc`, `NodeService` health/info/capacity; `delonix serve api` runs it | the three doc endpoints answer; spike result on REST transcoding recorded |
| D | The D2 conventions as one layer (problem+json, `ETag`/`If-Match`, `Link`, pagination, idempotency) and `links` in the contract | conformance suite covers each |
| E | Coverage waves over the contract (containers, VMs, networks, volumes, images, operations, stacks), each mapped `delonix-mgmt` path migrated | every row of the discovery map is served, refused with a reason, or marked missing |
| F | Remove `delonix-mgmt` | no mapped path left; `dashstats` moved to the application layer |
| G | Docs completeness gate at a true baseline, ratcheting down to zero | gate green at zero |

## What this ADR does not decide

- **How REST is transcoded from the `google.api.http` annotations.** No Rust crate does it
  the way grpc-gateway does in Go. Step C spikes the options — a build-time generator of
  `axum` routes from the annotations, or hand-written routes calling the same service
  implementation under a test that compares them with the OpenAPI — and records the result.
  **Decided by the step C spike — see «Spike result: REST transcoding» below.**
- **The stability tier of each service** (ADR-0041 D3).
- **The manifest `apiVersion`** (D1).

## Consequences

- One number and one door for anyone integrating: `/v1`, `delonix serve api`, `/docs`.
- `delonix-mgmt`'s clients migrate before step F; it was never declared stable.
- A library consumer of `delonix-image` that used the HTTP helpers keeps its own copy.
- The contract grows a `Link` message and a `links` field on resources and `Operation`
  (additive; `buf breaking` stays green).

## Spike result: REST transcoding (step C, 2026-10-03)

The first option won: **a build-time generator**. `delonix-node-api/build.rs` reads the
descriptor set the stubs are built from and writes one row per RPC that carries a
`google.api.http` rule (method, path template, body rule, the request fields a query string
may bind) and one dispatcher per service; `transcode.rs` is the part that is the same for every
RPC — matching, binding path, query and body into the request message, calling the service
method the gRPC encoding calls.

What the spike measured, and what decided it:

- **axum cannot spell the contract's paths.** 23 of the 57 mapped RPCs use a custom verb
  (`/v1/namespaces/{namespace}/containers/{name}:start`, `/v1/images:pull`), and axum 0.7
  matches a parameter per whole segment. Hand-written axum routes were not an option for those;
  the generated table has its own matcher, where a plain `{name}` never takes a value with a
  `:` in it, so `…/web:start` is the `:start` route and never the container named `web:start`.
- **No new dependency.** `google.api.http` is extension 72295728 of `MethodOptions`, which
  `prost-types` drops; the generator declares the few descriptor messages it needs with `prost`
  (already in the tree) and that tag as an ordinary field.
- **The whole contract is in the table, served or not.** A route of a service this engine does
  not serve yet answers `501` with DX-6001; a path the contract does not have answers `404`
  with DX-4001; a contract path on another method answers `405` with the methods the contract
  maps. Before this, every unserved route was a 404, indistinguishable from a typo.
- **Binding follows `google/api/http.proto`**, and refuses what it does not understand: an
  unknown query parameter, a query parameter on a `body: "*"` route, a body on a route without
  one, a body field that disagrees with the path, `true`/`false` and numbers checked against the
  field's type.
- **The comparison the second option wanted is kept as a test**: the generated table and the
  published OpenAPI list the same 57 operations, method for method and path for path.

Not carried by the REST encoding yet: the three server-streaming RPCs (`WatchEvents`, logs, operation
watch) answer `501` and name gRPC, which serves streams on the same socket. `additional_bindings`
fail the build instead of being dropped.

## Slice E2: the operation record (step E, 2026-10-03)

**What is served.** `POST /v1/namespaces/default/networks` (`CreateNetwork`),
`DELETE /v1/namespaces/default/networks/{name}` (`DeleteNetwork`),
`GET /v1/operations/{id}` and `GET /v1/operations`, on both encodings.

**The record.** One file per operation, `<state root>/operations/<id>.json`,
written as `RUNNING` before the work starts and rewritten when it ends. It names
the process doing the work (pid and start time). A reader that finds an
unfinished record whose owner is gone ends it `FAILED` with reason `Interrupted`
and writes that back — the server is socket-activated and has no supervisor to
do it. A finished record is removed seven days after it ended, when a new
operation begins.

**What is refused before an operation exists, and what is the operation.** What
can be decided without touching anything is the error itself, and writes no
record: a request that is wrong (name, subnet, namespace, topology), a name
already taken (`ALREADY_EXISTS`, 409), an `etag` that does not match (`ABORTED`,
412 — corrected in slice E3 to `FAILED_PRECONDITION`, the code `ResourceMeta.etag` names), a network something is attached to (`FAILED_PRECONDITION`, 409, DX-5307,
naming what is attached). A failure of the work is the operation, `FAILED`, with
the engine's class in `error.reason` and its number in `error.metadata["dx"]`.

**Idempotency.** A `request_id` gives the operation the id `r-<request_id>`,
created exclusively (`link(2)`): of two requests with one `request_id`, one
begins. The same request sent again is answered with that operation, whatever
its state — before the checks the first request itself made fail (a create's
"already exists", a delete's "not found"). A `request_id` that already answered
another verb or target is refused. It is 1 to 64 characters of letters, digits,
`.`, `_` and `-`, because it is part of a file name. On REST the
`Idempotency-Key` header is the request's `request_id`, and `If-Match` its
`etag`; a header and a body field that disagree are refused.

**The REST answer.** The `Operation`, with `Location: /v1/operations/{id}`:
`202 Accepted` while it runs, `200` once it has ended. Both network mutations
end before they answer, so they answer `200` — a client reads `state`, not the
status. The table in D2 says `201 Created` for a `POST` on a collection; an RPC
that returns an `Operation` does not answer the resource, so it does not answer
`201`. An operation that created something links it as `target`.

**One write path.** The work is `delonix_sdn::netops` — the declarative record,
then the dataplane record, the rollback when the second fails, and the in-use
check — and the CLI's `network create`/`network rm` go through the same
functions. Only the `bridge` topology is created through the API; `overlay` is
refused by name (501).

**A defect this found.** `NetworkStore::create_with_cidr` did not check the
name: measured on v4.4.0, `delonix network create '../evil' --subnet
10.231.0.0/24` wrote the record outside `networks/` and exited 0. The check
moved into the store (`NetworkStore::validate_name`), with a test, before a
name could arrive by socket.

**Not validated.** A server killed in the middle of a create, on a real socket:
the `Interrupted` path is proven with a record written by hand whose owner is a
process that exited. `Operation.result` (the typed result as `Any`) and
`trace_id` are left empty.
