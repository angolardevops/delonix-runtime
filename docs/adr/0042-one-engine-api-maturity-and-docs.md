# ADR-0042: One engine API — one version number, Richardson maturity, published docs

- **Status:** Accepted (2026-09-17) — steps A and B delivered (#388, #389); step C and onwards wait for the node API server being built in another session, and follow this ADR when they land
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
- **The stability tier of each service** (ADR-0041 D3).
- **The manifest `apiVersion`** (D1).

## Consequences

- One number and one door for anyone integrating: `/v1`, `delonix serve api`, `/docs`.
- `delonix-mgmt`'s clients migrate before step F; it was never declared stable.
- A library consumer of `delonix-image` that used the HTTP helpers keeps its own copy.
- The contract grows a `Link` message and a `links` field on resources and `Operation`
  (additive; `buf breaking` stays green).
