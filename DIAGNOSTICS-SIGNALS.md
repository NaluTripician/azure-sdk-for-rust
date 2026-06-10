# Diagnostics signal analysis (TSG-driven)

> **Purpose.** Ground the "what belongs in the cheap summary vs. the full detail" decision in how
> the SDK *already* treats diagnostics, so Combo 2's reducer and Combo 1's attribute set surface
> the right things. Every signal below cites in-repo evidence.

## Method

There is no standalone TSG document checked into this repo, so the strongest in-repo evidence of
"what support and the SDK actually rely on" is:

1. **What the core SDK already emits as telemetry.** The HTTP instrumentation policy decides which
   attributes are worth attaching to every request span — that list *is* the SDK's high-value set.
2. **What Cosmos thresholds on.** `DiagnosticsThresholds` encodes the exact dimensions Cosmos uses
   to decide when an operation is worth surfacing — i.e. the signal-gated fields.
3. **The Cosmos `DiagnosticsContext` shape.** The driver's architecture doc spells out the
   per-operation / per-request diagnostics model support reads.

### Sources

- `sdk/core/azure_core/src/http/policies/instrumentation/mod.rs` — the emitted attribute keys.
- `sdk/core/azure_core/src/http/policies/instrumentation/request_instrumentation.rs` — which
  attributes are set, and when (note: `error.type` is set for **all** 4XX/5XX, status code is set
  on every response, `http.request.resend_count` on retries).
- `sdk/cosmos/azure_data_cosmos/src/constants.rs` — Cosmos response header names
  (`x-ms-request-charge`, `x-ms-substatus`, `x-ms-activity-id`, `x-ms-retry-after-ms`,
  `x-ms-item-count`, `x-ms-resource-quota`/`-usage`, query-metrics headers).
- `sdk/cosmos/azure_data_cosmos_driver/src/options/diagnostics_thresholds.rs` — latency
  (point/non-point), request-charge, and payload-size thresholds.
- `sdk/cosmos/azure_data_cosmos_driver/ARCHITECTURE.md` §"Diagnostics Context" — `activity_id`,
  `duration`, `status_code`, `sub_status_code`, per-request `execution_context`
  (Initial/Retry/Hedging/RegionFailover/CircuitBreakerProbe), `region`, `endpoint`,
  `request_charge`, `duration_ms`, `request_sent` (Sent/NotSent/Unknown), and transport `events`
  (TransportStart/ResponseHeadersReceived/TransportComplete/TransportFailed).

## High-value signals (ALWAYS in the summary)

Ranked by how directly they drive a support investigation.

| Rank | Signal | Why (and citation) | In our spike |
|---|---|---|---|
| 1 | **Final status code** + per-attempt status mix | Status is set on every response span (`HTTP_RESPONSE_STATUS_CODE_ATTRIBUTE`, request_instrumentation.rs:172) and is the first thing any TSG branches on. | `Summary::status_counts`, `outcome` |
| 2 | **Error type / classification** | `error.type` is set for every 4XX/5XX (request_instrumentation.rs:168,184). Cosmos adds `sub_status_code` for finer classification (ARCHITECTURE.md:316,332). | `Summary::top_error{status,error_kind}`; combo1 `az.error_kind` |
| 3 | **Service request id** (`x-ms-request-id` / Cosmos `activity_id`) | The id a customer pastes into a ticket; emitted as `az.service_request.id` (mod.rs:15) and is the `activity_id` root of `DiagnosticsContext` (ARCHITECTURE.md:313). Captured from the **response**. | `Summary::final_service_request_id`; combo1/2 `az.service_request_id` per attempt |
| 4 | **Request charge (RU)** | A first-class **threshold** (`request_charge_threshold`, diagnostics_thresholds.rs:15) and per-request field (ARCHITECTURE.md:333). RU is the cost/throttle signal for Cosmos. | `Summary::total_request_charge` (+ `high_charge` when over threshold) |
| 5 | **Retry / resend count** | Emitted as `http.request.resend_count` on retries (request_instrumentation.rs:155); `execution_context = Retry` is a per-request state (ARCHITECTURE.md:324). | `Summary::retry_count`, `attempt_count` |
| 6 | **Throttling (429) occurrence** | 429 is the canonical retry/backoff trigger ("Retry after 429/503", ARCHITECTURE.md:324; retry policy lists `TooManyRequests`, retry/mod.rs:144). Must never be hidden. | `Summary::throttle_count` + `status_counts["429"]` |
| 7 | **Total elapsed / operation duration** | Top-level `duration` of `DiagnosticsContext` (ARCHITECTURE.md:314); latency is thresholded (diagnostics_thresholds.rs:13–14). | `Summary::total_elapsed_ns` |
| 8 | **Endpoint / region** | Per-request `region` + `endpoint` (ARCHITECTURE.md:329–330); `server.address`/`url.full` core attrs (mod.rs:19,21). Essential for cross-region failover analysis. | combo1/2 `az.endpoint` (region modeled as future work) |

## Detail-only / threshold-gated signals (full view, or surfaced only when notable)

| Signal | Why detail-only (citation) | In our spike |
|---|---|---|
| **Per-attempt transport duration** | Useful but noisy; only matters when slow. Cosmos gates on a latency threshold (diagnostics_thresholds.rs:13–14). | combo2 surfaces `slow_attempt_ns` only above threshold; full per-node `duration_ns` lives in the detailed binary |
| **Per-request `request_sent` (Sent/NotSent/Unknown)** | Critical for retry-safety reasoning but only on the error path (ARCHITECTURE.md:335–338). | detailed tier (future attribute; not modeled in scenarios) |
| **Transport events** (TransportStart/ResponseHeadersReceived/TransportComplete/TransportFailed) | Fine-grained, high-volume; reqwest can't even split DNS/TLS/connect (ARCHITECTURE.md:357–375). Belongs in full detail. | detailed binary tree (Combo 1/2 node kinds) |
| **Sub-status code** | Finer error classification; only needed once you know there's an error (ARCHITECTURE.md:316,332; `x-ms-substatus`, constants.rs:47). | extend `top_error` / attempt attrs when modeled |
| **Fan-out children** (plan node + feed range) | One row per partition/range — explodes size. A *count* belongs in summary; the per-child detail in the full tree. | combo2 `child_count` in summary; children in detailed binary; combo1 `az.plan_node_id`/`az.feed_range` |
| **Payload size** | Thresholded (diagnostics_thresholds.rs:16), not needed inline normally. | future threshold-gated field |
| **Quota / usage, query metrics** | Diagnostic deep-dive headers (`x-ms-resource-quota`/`-usage`, query-metrics; constants.rs:103–106). | detailed tier only |

## How this fed back into the prototypes

- **Combo 2 reducer (`summary.rs`).** Implements the high-value list as always-present fields and
  the threshold-gated list via `SummaryThresholds`: `slow_attempt_ns` (latency gate) and
  `high_charge` (RU gate, mirroring `request_charge_threshold`). The 429/throttle count and status
  histogram are always surfaced, per signals #1 and #6.
- **Combo 1 attribute set (`lib.rs`/`arena.rs`).** Already records the high-value per-attempt
  attributes — `az.status_code`, `az.service_request_id`, `az.request_charge`, `az.error_kind` —
  plus `az.plan_node_id`/`az.feed_range` for fan-out and `az.attempt_count`/`az.outcome` on the
  root. No change needed; region / sub-status / `request_sent` are the natural next attributes once
  the scenarios model them.

## Naming note for ratification

The core SDK emits the service request id as **`az.service_request.id`**
(instrumentation/mod.rs:15), while these spikes use the underscored canonical **`az.service_request_id`**.
The exact key (and the RU key, e.g. `az.request_charge`) is one of the cross-SDK items to ratify —
see `DIAGNOSTICS-CROSS-SDK.md`.
