# Combo 3 — tracing-native baseline (FINDINGS)

**Stack:** A2 span hierarchy · B2 `tracing`-crate-native · C1 eager structured objects · D1 JSON.
**Role:** the strawman / baseline every other combo is benchmarked against.

## What it is

An outer `operation` span wraps the retries; each HTTP attempt is an `attempt` span; fan-out
produces `routing` child spans. Request/response/error are `tracing` events. A capturing
`tracing-subscriber` layer (`SpanCollector`) eagerly records every span, field, and event into a
store, which is then projected into typed objects (`OperationSpan`) and serialized to JSON.

## Ergonomics

- **Excellent build ergonomics.** It is the least code to stand up: spans + events are idiomatic
  `tracing`, and any existing `tracing` subscriber/exporter "just works" alongside the capture
  layer. No bespoke storage to design.
- **Field plumbing is slightly awkward.** Deferred fields (`attempt_count`, `outcome`,
  `duration_ns`) must be declared `Empty` on the span and `record`-ed later. Dotted attribute
  names (`az.service_request_id`) are supported.
- `Option` is not a `tracing::Value`, so optional fields are written with defaults at the call
  site (fine for the spike, where every scenario supplies the value).

## OTel alignment — strong

This is the design's biggest advantage. Because capture is literally `tracing` spans/events,
mapping to OpenTelemetry spans is direct, and reconstruction of an OTel trace is trivial. Any
team already on `tracing`/OTel inherits this for free.

## Happy-path cost — high (the headline weakness)

There is **no outcome-aware drop trick**. The subscriber processes and stores every span, field,
and event eagerly — on success exactly as much as on error. You pay:

1. **collect** — span creation + layer capture (mutex + `BTreeMap` inserts, JSON `Value` per field),
2. **construct** — projecting the raw store into the typed `OperationSpan` graph,
3. **serialize** — `serde_json` over a nested object tree.

All three are paid on the happy path. (See `DIAGNOSTICS-BENCH.csv` for the measured numbers; this
is the row the other combos quote their reduction multipliers against.)

## Output

Human-readable JSON. Verbose by construction (every attribute is a quoted key/value; S4 fan-out
balloons because each child is a full object). This is what makes it the worst case for output
size and the clearest contrast with the binary combos.

## Getting a programmatic object out

You **need the capture layer** — plain `tracing` only emits to subscribers. The `SpanCollector` +
`project` step is what turns the ephemeral span stream into an inspectable `OperationSpan`. That
is extra machinery a pure-`tracing` shop would not otherwise have.

## Verdict

Keep as the baseline and as the OTel-export path. Not the right default for cost-sensitive,
high-volume operations because of the unconditional eager cost and large JSON.
