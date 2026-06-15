<!-- Copyright (c) Microsoft Corporation. All rights reserved. Licensed under the MIT License. -->

# Examples

## `diagnostics_demo` — gated diagnostics, offline walk-through

A fully **offline**, presentation-ready demo of the Cosmos driver's gated diagnostics engine
(`azure_data_cosmos_driver::diagnostics::capture`). It builds each scenario from the public capture
API — the same front-end the driver uses on the hot path — and pretty-prints the resulting canonical
`DiagnosticsContext`. **No live Cosmos account is required.**

Run it:

```bash
cargo run -p azure_data_cosmos_driver --example diagnostics_demo
```

### What it shows (one labeled section each)

1. **Typical single-attempt success** — a `200`: activity id, status, request charge, region,
   endpoint, and per-attempt server timing.
2. **Retry after throttling (429 → 200)** — two `RequestDiagnostics` with
   `ExecutionContext::Initial` (carrying the throttle sub-status `3200`) then `Retry`.
3. **Error operation** — a terminal failure: final status + sub-status and the service request
   (activity) id captured on the error path.
4. **Hedged multi-region** — the per-region legs, which leg won, and the `HedgeDiagnostics`
   terminal state (`AlternateWon`).
5. **Gate modes side by side** — the same operation under `Off`, `Always`, and `Threshold` (plus a
   `Threshold` fast-success that is dropped), and the `should_build` gate predicate truth table.

### Live-demo narration (optional)

> "Every Cosmos operation produces a `DiagnosticsContext`. Section 1 is the happy path — one
> request, its RU charge, region and timing. Section 2 shows a throttle: the diagnostics keep
> *both* attempts, tagged `Initial` (429/3200) then `Retry` (200). Section 3 is an error — the
> final status, sub-status and service request id are all captured. Section 4 is cross-region
> hedging: two legs race, West US wins, and the `HedgeDiagnostics` records the terminal state.
> Section 5 is the gate: `Off` produces nothing, `Always` always builds, and `Threshold` only
> surfaces slow or errored operations — so a fast success is dropped for ~free."

The canonical JSON for each context exposes every rich field slot (`events`, transport-shard,
`fault_injection_evaluations`, sub-status, …); the synthetic offline scenarios fill the
operation/attempt-level fields, while `events` and transport-shard detail are populated by the live
pipeline and appear here as their empty slots.
