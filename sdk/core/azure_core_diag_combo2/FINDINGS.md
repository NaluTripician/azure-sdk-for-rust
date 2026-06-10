# Combo 2 — Tiered hybrid, migration-friendly (FINDINGS)

**Stack:** A3 hybrid · B3 wrapper types · C3 outcome-aware · D3 tiered verbosity
(default summarized JSON, detailed = binary).
**Role:** primary candidate. The customer- and migration-friendly path.

## What it is

The capture mechanism is a **wrapper sink (B3)**: each emit call-site dual-writes — it emits an
internal `tracing` event (so any existing `tracing`/OTel consumer keeps working) *and* records into
a lightweight retained model. Two views are produced from that model (A3 + D3):

- **Summary (default):** a small, request-focused, aggregatable JSON object — status histogram,
  throttle count, total RU, retry count, total elapsed, the top error, final service request id.
- **Detailed (on demand / on error):** the full span tree serialized to the shared `AZD1` binary
  blob (the same wire format and decoder as Combo 1).

## Migration story (the key selling point)

The summary is intentionally shaped like **today's request-style diagnostics**: a flat,
aggregatable record a customer or support engineer already knows how to read, and that rolls up
across many operations. Teams can adopt Combo 2's default with essentially no change to existing
dashboards/TSGs, then opt into the detailed binary where they need full fidelity. This is the
lowest-friction path to the new format and the natural second adopter after Combo 1's wire format
lands.

## Outcome-aware tiering (C3) + the granularity knob

- `Verbosity::{Summary, Detailed}` is the explicit knob.
- `effective_verbosity` applies the **escalate-on-error** rule: a `Summary` request is auto-upgraded
  to `Detailed` when the operation failed, so error diagnostics are never lossy (S3 always gets the
  full tree). Success stays cheap (summary only).
- On success with `Summary`, the detailed tree is never built or serialized — only the small
  summary is produced.

## Reducer quality & where the threshold lives

`reduce(captured, thresholds)` projects the detailed model into the summary. Rules are seeded from
`DIAGNOSTICS-SIGNALS.md`:

- **Always surfaced** (high-value): status histogram (so the 429 count is always visible),
  `throttle_count`, `total_request_charge`, `retry_count`, `final_service_request_id`, and the
  first `top_error`.
- **Conditionally surfaced** (detail-only unless notable): `slow_attempt_ns` appears only when an
  attempt exceeds `SummaryThresholds::slow_attempt_ns` (default 5 ms). S1's 4 ms point read stays
  quiet; S4's 6 ms attempt is flagged.

The single tuning knob (`SummaryThresholds`) is where product/TSG owners decide how chatty the
summary is. It is the obvious place to extend with more signal-gated fields as TSG analysis grows.

## Cost shape

- **collect** — record into the retained model + a (no-op-by-default) `tracing` event per call.
- **construct/serialize, summary mode** — reduce + compact JSON. Cheap and bounded (independent of
  fan-out width — S4's 25 children collapse to a single `child_count`).
- **construct/serialize, detailed mode** — build the wire tree + binary encode (≈ Combo 1).

So the happy-path default pays collect + a small summary, never the full tree. See
`DIAGNOSTICS-BENCH.csv` for both modes side by side.

## Verdict

Best default for customers: cheap, readable, aggregatable summary by default; full binary detail
exactly when it is worth it (on error or on request). Pairs naturally with Combo 1 — it reuses the
same binary wire format for its detailed tier, so the two are complementary rather than competing.
