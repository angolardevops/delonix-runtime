# The engine's own SLIs, measured without a daemon (M08, design note)

Date: 2026-10-12. Status: design note, nothing wired. Roadmap item M08
(`docs/roadmap/13-improvements-traceability.md`) stays `NOT_STARTED` until one
of the SLIs below is exposed and has a reference panel.

## The constraint

The engine is daemonless: no process is awake between two commands to keep a
time series. So an SLI here is either

1. **derived from a durable record the engine already writes** (the event log
   `<root>/events.jsonl`, the operation ledger `<root>/operations/*.json`), by
   whoever reads it — a scraper, a `system` command, a dashboard; or
2. **a counter or histogram owned by a process that is already resident** for
   another reason (the `delonix-cri` server under a kubelet, a socket-activated
   `delonix-node-api` while it is up), and therefore valid only for the
   requests that process served.

Nothing here justifies a new resident process. A metric that needs one is
listed as not measurable and says why.

## The two durable sources, as they are today

| Source | What it records | Time resolution | Retention |
|---|---|---|---|
| Event log (`delonix_node::events`) | one line per lifecycle event: `container create/start/stop/die/crashed`, `stack apply-started/apply-done/apply-failed` (per Kind layer), `volume create/remove`, `secret remove`, policy grants | **seconds** (`ts`) | one rotation generation, 4 MiB ceiling |
| Operation ledger (`delonix-node-api::operations`) | one record per node-API mutation: `state`, `created_ms`, `ended_ms`, `failure.reason` | **milliseconds** | 7 days for a finished record |

Two properties matter for SLIs. The `die` event carries the **real** exit code
(`exit=<n>`), written by the supervisor, the only process that knows it. And a
killed `stack apply` leaves `apply-started` with no closing line, which is
itself countable.

## SLIs that can be computed now

| SLI | Source | Definition | Good event | Notes |
|---|---|---|---|---|
| **Apply success ratio** | event log | per Kind layer: `apply-done / (apply-done + apply-failed)` over a window | `apply-done` | An `apply-started` with no closing line older than the window is counted as bad (a killed apply). Measures the layers, not whole stacks: the log has no stack-level closing line. |
| **Node-API mutation success ratio** | ledger | `Succeeded / (Succeeded + Failed)` over `created_ms` in the window | `Succeeded` | `Failed` with reason `Interrupted` is an engine fault (the worker died); a `Failed` with an engine class from the request (conflict, not found) is a caller error and is **excluded** from the denominator, or the SLI measures the users. |
| **Node-API mutation latency** | ledger | distribution of `ended_ms - created_ms` per `verb` | under a threshold per verb | Millisecond resolution; only operations that went through the node API. |
| **Unexpected container death rate** | event log | `die` with `exit` ∉ {0, 143} and no preceding `stop` of the same id, plus `crashed`, per container-hour | — | Needs a `stop`→`die` pairing per id. `restart_counts` (the `RESTARTS` column) only counts `start` events and does no pairing, so this is new code — put it in the reader of step 2 below, and move `restart_counts` onto it, so there is one reading of the log, not two. |

## SLIs that cannot be computed from today's records

| SLI | Why not | What would make it measurable |
|---|---|---|
| **`container run` latency** (the one the maturity plan names first) | the event log has second resolution and no event at the moment the command started; a `run` takes ~90–100 ms (measured in the `spawn()` split, 2026-10-12) | a `create` event carrying the elapsed milliseconds in `detail` (e.g. `ms=95`), written by the CLI right after `spawn` returns. Short, fixed field: it keeps the line under `PIPE_BUF`. |
| **Holder recovery time** | the network infra emits no events: a pin rebuild or a control-plane restart leaves no line | two infra events, `network holder-rebuild` and `network control-restart`, with the elapsed milliseconds and the count of workloads recovered in `detail`. |
| **Image pull latency and failure ratio** | `pull` is not in the event log; the `delonix_images_pulled_total` counter exists only inside the CRI process | a `image pull` event with the outcome and bytes. |

## Where a scrape would read them

The shared registry is `delonix-telemetry::metrics`. Its resource gauges are
already *recomputed by the caller right before a scrape*, which is exactly the
shape a log-derived SLI needs. But today the only scrape surfaces are:

- `delonix-cri` `/metrics` — out of scope for this sprint, and it would only
  see the CRI's own requests;
- `delonix-mgmt` `/metrics` — frozen by ADR-0041 D4 (bug and security fixes
  only), so adding SLI gauges there is new surface on a frozen crate;
- `delonix-node-api` — no `/metrics` route yet.

**Decision for this sprint: no gauges were added.** A gauge registered in
`delonix-telemetry` with no caller that sets it would be dead code that reads as
a measured zero on a scrape, which is the failure this project removes
elsewhere (`unknown is never zero`). The gauges enter together with their first
caller.

## Proposed order

1. Add `ms=<n>` to the `container create` event (CLI, one field), and the two
   infra events. Small, and it turns the two most-asked SLIs from «not
   measurable» into «derivable».
2. A pure reader in `delonix-node` (`events::sli_window(root, since) -> Sli`)
   that computes the four SLIs above from the log and the ledger. Tested against
   a written log, no process.
3. A `GET /metrics` on `delonix-node-api` (ADR-0042 lists `/metrics` among the
   documentation and scrape paths of the node API), which calls that reader and the ledger before
   rendering, and sets gauges in the shared registry. Socket-activated: the
   SLI exists only while something scrapes it, which is the daemonless answer.
4. A reference Grafana panel in `docs/`, then M08 moves to `PARTIAL`.

## What this note does not decide

SLO targets and error budgets are the operator's, not the engine's: the engine
publishes the indicator, a consumer chooses the objective.
