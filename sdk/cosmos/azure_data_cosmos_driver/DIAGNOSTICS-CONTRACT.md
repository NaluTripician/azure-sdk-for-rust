<!--
Copyright (c) Microsoft Corporation. All rights reserved.
Licensed under the MIT License.
-->

# The driver↔SDK diagnostics contract & its OpenTelemetry mapping

> **Status: design (contract-first).** This is the primary diagnostics document for
> `azure_data_cosmos_driver`. It defines the **contract** between the driver and its
> consumers (the Rust `azure_data_cosmos` SDK today; the next-major Go/Python SDKs over
> FFI) and the **OpenTelemetry customer experience**. The append-only *capture engine*
> described in [`DIAGNOSTICS-CAPTURE.md`](./DIAGNOSTICS-CAPTURE.md) is a **deferred
> optimization** kept OFF by default (`capture_engine` feature) — it is an implementation
> detail *under* this contract, not the contract itself.
>
> This document is design-only: it does not add or change public API. Where it sketches a
> future surface, that surface is **additive and non-breaking** and gated behind team
> decisions recorded in [§9](#9-resolved-decisions).

## 1. Why a contract first

The driver already produces rich per-operation diagnostics and hands them back through a
cheap handle. Two things were underspecified, and the SDK requirements must drive the
driver design (not the other way around):

1. **One implicit shape.** Materialization is JSON-only and implicit. Different consumers
   want different shapes: a metrics pipeline wants a structured object, a tracing backend
   wants spans, a log sink wants a string. JSON is the single costliest step
   (bench: struct build ~3.7 µs → +detailed JSON ~6.6 µs), so forcing it on everyone is
   wasteful — and lossy/expensive across an FFI boundary.
2. **No agreed boundary for non-Rust consumers.** The next-major Go/Python SDKs consume the
   driver over FFI. A fixed serialized format at that boundary is the wrong default.

So we fix the **contract** and the **OpenTelemetry mapping** first; the hot-path capture
engine is a deferrable optimization.

## 2. The contract in one picture

```text
driver operation completes
        │
        ▼
   DiagnosticsHandle          ← cheap: today's Arc<DiagnosticsContext> (an atomic incr).
        │                        ALWAYS available. Diagnostics are ALWAYS collected.
        │  materialize on demand — pay only for the shape you ask for:
        ├──▶ as_metrics(level) → structured object   (metrics)
        ├──▶ as_spans()        → OTel span tree        (traces)
        └──▶ as_log(encoding)  → String                (logs: JSON / compact / encoded)
```

Three invariants:

- **P1 — The handle is cheap and unconditional.** In Rust it *is*
  [`CosmosResponse::diagnostics()`][diag] → `Arc<DiagnosticsContext>`. Cloning it is an
  atomic increment. It is **always** returned; `diagnostics()` is **non-optional**.
- **P2 — Materialization is explicit, lazy, per-representation.** No single fixed serialized
  format. Each materializer is paid only when called and cached, so the expensive JSON step
  is never paid on a metrics-only or span-only path.
- **P3 — The level/threshold governs EXPOSURE and DEPTH, never COLLECTION.** A diagnostics
  *level* bounds the high-cardinality **transport-level** detail a materialization includes;
  it never removes operation-level diagnostics and never disables collection.

There is **no parallel diagnostics model**: the driver owns one canonical
[`DiagnosticsContext`][ctx] and every representation is a view over it.

## 3. The three representations (views over one model)

| Materializer | Consumer intent | Backed by (exists today) |
|---|---|---|
| **Structured object** | metrics | [`DiagnosticsContext::summary()`][summary] (operation roll-up) + [`requests()`][requests] (per-attempt) |
| **OTel span tree** | traces | the `diagnostics::capture::event` `Span`/`Attr`/`SpanKind`/`TimeOffset` model + per-op/attempt wall-clock timestamps |
| **String** | logs | [`DiagnosticsContext::encode(DiagnosticsEncoding)`][encode] / `to_json_string(verbosity)` |

## 4. The FFI boundary (Go / Python next-major)

**How diagnostics flow over FFI: an opaque handle + explicit materialize calls. Never force
full JSON at the boundary.**

```text
Rust driver                         FFI (C ABI)                    Go / Python SDK
───────────                         ───────────                    ───────────────
Arc<DiagnosticsContext>  ── boxed →  DiagnosticsHandle (opaque ptr) hold cheaply; free later
                                     cosmos_diag_materialize(
                                       handle, representation,       caller picks the shape
                                       level, out_buf)
                                       ├ Metrics → packed struct / flatbuffer
                                       ├ Spans   → span-tree buffer (or per-span callback)
                                       └ Log     → UTF-8 bytes (JSON / compact / encoded)
                                     cosmos_diag_free(handle)        explicit lifetime
```

Contract rules for the boundary:

1. **Opaque handle.** The FFI surfaces an opaque pointer wrapping the `Arc`. Crossing the
   boundary is a pointer move + refcount, **not** a serialization.
2. **Explicit materialize, consumer-chosen shape.** One `materialize(handle, representation,
   level)` entry point where `representation ∈ {Metrics, Spans, Log}` and `level` selects
   transport-detail depth.
3. **No forced JSON.** JSON is one representation requested explicitly; a Go metrics exporter
   never triggers it.
4. **Explicit lifetime (resolved: opaque handle + `free`).** The handle is freed by an
   explicit `cosmos_diag_free`; refcounting stays on the Rust side. A scoped
   materialize-then-drop callback may be added later as a convenience wrapper.
5. **Bounded output.** Every materialization honors the bounded-size guarantee ([§6](#6-bounded-size-guarantee-retry-storms)),
   so an FFI buffer can be sized predictably even under a retry storm.

The exact wire encoding of each representation (packed struct vs flatbuffer vs span callback)
is a follow-up implementation detail; the contract fixes only "opaque handle + explicit
per-representation materialize + bounded output".

## 5. Where levels / thresholds apply

**Gating bounds high-cardinality TRANSPORT-level telemetry — it never eliminates
diagnostics.**

| Tier | Examples | Cardinality | Gating |
|---|---|---|---|
| **Operation-level** | operation name, final status, request/retry/throttled counts, total RU, total duration, regions contacted | low | **Always on.** Never gated away. |
| **Transport-level** | per-replica / per-partition (`partition_key_range_id`, `feed_range`), endpoint, direct-mode channel, `transport_kind`/`security`/`http_version`, `transport_shard`, per-attempt RU/latency | high | **Gated by `DiagnosticsLevel` / threshold.** Included on error / slow / high level; summarized or elided on a fast-success low level. |

The knob is a dedicated **`DiagnosticsLevel { Minimal, Standard, Full }`** (resolved: a new
enum rather than overloading [`DiagnosticsVerbosity`][verbosity], which is string-render
specific and has no `Minimal`). `DiagnosticsLevel` maps onto `DiagnosticsVerbosity`
internally:

- `Minimal` — operation-level only.
- `Standard` — + region-grouped/deduplicated transport summary (`Verbosity::Summary`).
- `Full` — + every per-attempt transport record (`Verbosity::Detailed`).

> **Collection is not gated.** Operation-level metrics
> ([`DiagnosticsSummary`][summary]) are *computed by iterating the per-attempt records*, so
> "cheap op-level only" is not achievable by dropping transport collection. The driver
> therefore **always collects the full per-attempt records** (the ~3.7 µs struct build) and
> the level gates only *materialization + exposure*. See [§9](#9-resolved-decisions) Q1.

## 6. Bounded-size guarantee (retry storms)

**Every materialized representation has an upper bound on size that is independent of attempt
count**, so a `410`/`429` retry storm or a large fan-out query cannot produce an unbounded
object, span tree, or string.

- **Mechanism (primitive already present).** `DiagnosticsVerbosity::Summary` groups requests
  by region, keeps first + last per region in full, deduplicates the middle by
  `(endpoint, status, sub_status, execution_context)` with count + min/max/P50, bounded by
  [`DiagnosticsOptions::max_summary_size_bytes`][maxbytes] (default 8 KB, min 4 KB).
- **Contract (resolved: configurable per-representation caps with documented defaults).**
  - max attempts rendered in the object (default 64), max spans in the tree (default 128),
    max bytes in the string (default 8 KB) — each overridable via `DiagnosticsOptions`.
  - Compaction is lossy only in the *middle* of a run; the head/tail extremes and the
    aggregates (counts, histogram, min/max/P50) are always exact.
  - Truncation is marked, never silent.

This document defines the guarantee; the append-only compaction engine that realizes it
cheaply is the deferred optimization in [`DIAGNOSTICS-CAPTURE.md`](./DIAGNOSTICS-CAPTURE.md).

## 7. OpenTelemetry mapping

### 7.1 Operation-level metrics (always-on, low cardinality)

Source: [`DiagnosticsSummary`][summary]. Emit as OTel metrics with only low-cardinality
attributes (`db.operation`, `db.cosmosdb.status_code`, op-granularity `region`):

| Instrument | Kind | Source | Attributes |
|---|---|---|---|
| `db.cosmosdb.operation.duration` | histogram | `total_duration_ms` | `db.operation`, `db.cosmosdb.status_code` |
| `db.cosmosdb.operation.requests` | counter | `request_count` | `db.operation` |
| `db.cosmosdb.operation.retries` | counter | `retry_count` | `db.operation` |
| `db.cosmosdb.operation.throttled` | counter | `throttled_count` | `db.operation` |
| `db.cosmosdb.request_charge` | histogram (RU) | `total_request_charge` | `db.operation`, `db.cosmosdb.status_code` |
| status distribution | counter | `status_counts` | `db.cosmosdb.status_code`, `db.cosmosdb.sub_status_code` |

### 7.2 Transport-level → traces, never metric dimensions

Source: [`RequestDiagnostics`][req] and its `events`. `endpoint`, `partition_key_range_id`,
`feed_range`, per-attempt `region`, `transport_kind`/`security`/`http_version`,
`pipeline_type`, and `transport_shard` are **unbounded** in practice. Putting them on metric
attributes explodes time-series cardinality — so they go on **span attributes** (and per-
attempt RU/latency may be surfaced as span exemplars), gated by `DiagnosticsLevel`.
(`partition_key_range_id` / `feed_range` surface via the event model, `AttrKey::PartitionKeyRangeId` / `AttrKey::FeedRange`, not as direct `RequestDiagnostics` fields.)

### 7.3 Traces (span tree)

| `DiagnosticsContext` element | OTel span |
|---|---|
| operation (root) | root span, kind `Client`, name `Cosmos <operation>`; start = `start_time()`, end = start + `duration()` |
| each `RequestDiagnostics` (attempt / hedge leg) | child span, kind `Client`; start/end = its `start_time`/`end_time` |
| `RequestEvent` timeline | timed span **events** on the attempt span |
| `HedgeDiagnostics` | a `Hedge`-kind span with terminal state + regions |
| **aggregated multi-run op** (`aggregate_sub_operations`) | **a single synthetic operation root** spanning the first run's start to the last run's end, with each run's attempts as children (resolved: [§9](#9-resolved-decisions) Q9) |

### 7.4 Attribute alignment with `azure_core`

Reuse the `azure_core` span-attribute names so Cosmos spans correlate with `azure_core`-
emitted spans:

| Diagnostics field | Attribute name |
|---|---|
| `DiagnosticsContext::activity_id` | `az.client_request_id` |
| `RequestDiagnostics::activity_id` (per attempt) | `az.service_request.id` ⚠ **dot**, not underscore |
| status | `http.response.status_code` |
| retry index | `http.request.resend_count` |
| endpoint | `server.address` / `url.full` |
| namespace | `az.namespace` (`Microsoft.DocumentDB`) |
| error | `error.type` |

> ⚠ **Two `azure_core` gotchas (resolved: [§9](#9-resolved-decisions) Q5).** (1) The constant
> is `az.service_request.id` (a **dot** before `id`), while `az.client_request_id` uses an
> underscore. (2) These constants are **module-private** in `azure_core`
> (`http/policies/instrumentation/mod.rs`) and cannot be imported. Until `azure_core`
> exposes them, the mapping centralizes identical string literals in one Cosmos-local module
> and an `azure_core` issue tracks exposing public constants + fixing the naming
> inconsistency. Cosmos-specific attributes (RU, sub-status, partition key range) use a
> documented `db.cosmosdb.*` namespace with underscores.

## 8. The OTel-aligned event model & the SDK↔driver split

- **The `event.rs` model is the OTel-aligned representation — keep it.**
  `Span { kind: SpanKind::{Operation,Attempt,Hedge}, parent: Option<SpanId>, start/end:
  TimeOffset }` + typed `Attr`s is an OpenTelemetry span tree in all but the emitter. It is
  retained as the canonical in-memory shape **even though the append-log perf optimization is
  deferred and `capture_engine` stays OFF** — it is a data shape, not a hot-path commitment.
- **Retroactive spans are feasible** (proved by the throwaway `otel_spans_spike` feature): a
  *completed* `DiagnosticsContext` is reconstructed into a backdated span tree using the raw
  `opentelemetry` `SpanBuilder` (`with_start_time`/`with_end_time`), because the
  `azure_core::tracing` abstraction builds spans at "now" and has no backdating hook. See
  [`tests/otel_retroactive_spans_spike.rs`](./tests/otel_retroactive_spans_spike.rs).
- **SDK-vs-driver split (resolved: hybrid, default SDK-side emission).** The **driver
  produces and materializes** the `DiagnosticsContext` (and offers an opt-in exporter); the
  **SDK (or the opt-in driver exporter) emits** the public operation span + operation
  metrics. Default emission is SDK-side so there is exactly **one public span per
  operation** (avoids double-counting). Rule of thumb: *the driver produces and materializes;
  the SDK emits.*

## 9. Resolved decisions

| # | Decision | Resolution |
|---|---|---|
| **Q1** | `Mode::Off` vs always-collect | **Always collect** full per-attempt records; the `DiagnosticsLevel`/gate governs materialization + exposure only. `Off` is deprecated (removing it would be a public break — `Mode` is not `#[non_exhaustive]`), not a collection switch. A counters-only cheap tier is a possible later optimization. |
| **Q2** | FFI handle lifetime | **Opaque handle + explicit `free`** as the primitive; scoped-callback convenience may wrap it later. |
| **Q3** | Transport-tier gating knob | **New `DiagnosticsLevel { Minimal, Standard, Full }`**, mapping onto `DiagnosticsVerbosity` internally. |
| **Q4** | Bounded-size caps | **Configurable per-representation caps** with documented defaults (8 KB string / 128 spans / 64 attempt records); first+last-per-region + exact aggregates always retained. |
| **Q5** | `azure_core` constants | Constants are private + `az.service_request.id` uses a dot: **centralize identical literals in a Cosmos-local module now**, file an `azure_core` issue to expose public constants + fix naming, then switch. |
| **Q6** | SDK-vs-driver emission | **Hybrid, default SDK-side emission**; driver offers an opt-in exporter. |
| **Q9** | Aggregated span-tree shape | **Single synthetic operation root** with each run's attempts as children. |

## 10. Scope & guardrails

- **Additive / non-breaking.** The public boundary is exactly
  [`diagnostics::DiagnosticsContext`][ctx], consumed by `azure_data_cosmos`.
  `CosmosResponse::diagnostics()` stays non-optional. No SemVer break.
- **`capture_engine` stays OFF by default** and the `DiagnosticsContextBuilder` is untouched;
  the append-only hot path / `LogPool` is a deferred optimization
  ([`DIAGNOSTICS-CAPTURE.md`](./DIAGNOSTICS-CAPTURE.md)).
- **Diagnostics are always collected** — there is no full-disable mode; the level governs
  exposure, not collection.
- **No `azure_core` / `typespec_client_core` change** is made by this contract; any new
  public constant there is a proposal (Q5).

[diag]: ./src/models/cosmos_response.rs
[ctx]: ./src/diagnostics/mod.rs
[summary]: ./src/diagnostics/capture/model.rs
[requests]: ./src/diagnostics/capture/model.rs
[req]: ./src/diagnostics/capture/model.rs
[encode]: ./src/diagnostics/capture/model.rs
[verbosity]: ./src/options/diagnostics_options.rs
[maxbytes]: ./src/options/diagnostics_options.rs
