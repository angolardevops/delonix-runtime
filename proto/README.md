# Delonix node contract (`delonix.node.v1`)

**Status: draft — ADR-0040.** Nothing serves or consumes these files yet.

The contract between this engine and a node-local consumer (the control plane's node
agent, DevOps tooling on the node). Served — once phase P5 lands — by
`delonix serve api` on a **local unix socket**, as gRPC and as HTTP/JSON generated from
the same files. It is not a remote API (ADR-0010).

Two rules a review enforces on every change:

1. No tenant, account, plan, quota or billing field. `namespace` is the engine's
   isolation namespace.
2. No provider name in a field the caller writes; provider knobs go in
   `ProviderExtensions`, and a provider rejects keys it does not know.

Check locally:

```bash
protoc -I proto -I ~/.local/include --descriptor_set_out=/dev/null proto/delonix/node/v1/*.proto
```
