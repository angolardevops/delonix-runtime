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

Check locally:

```bash
protoc -I proto -I ~/.local/include --descriptor_set_out=/dev/null proto/delonix/node/v1/*.proto
```
