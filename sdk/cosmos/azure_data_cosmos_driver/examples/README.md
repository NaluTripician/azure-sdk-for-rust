<!-- Copyright (c) Microsoft Corporation. All rights reserved. Licensed under the MIT License. -->

# Examples

## `diagnostics_demo` — gated diagnostics, LIVE or offline

A presentation-ready demo of the Cosmos driver's gated diagnostics engine
(`azure_data_cosmos_driver::diagnostics::capture`). It runs in two modes.

### LIVE mode (real account + fault injection)

Connects to a real Cosmos account and uses the driver's **fault injection** to force the scenarios,
so the printed `DiagnosticsContext` carries **real** server timings, activity ids, and regions.
Requires the `reqwest` + `fault_injection` features.

```bash
cargo run -p azure_data_cosmos_driver --example diagnostics_demo --features "reqwest fault_injection"
```

It uses the `COSMOSDB_MULTI_REGION` account **only** (read from the env var at runtime; secret
values are **never** printed — only the endpoint host), authenticating with the master key from the
connection string.

LIVE mode creates a temporary database + container, seeds one item, runs the scenarios, and
**deletes the temporary database on exit**. Each scenario prints the real `DiagnosticsContext` plus
the Summary block, the encoding sizes (Json/Compact/Encoded), and the `FaultInjectionEvaluation`s
proving the injected fault fired:

- **A. 429 throttle → retry → success** — `TooManyRequests` injected on `ReadItem` with a hit-limit;
  the driver retries to a real `200`.
- **B. 503 server error** — `ServiceUnavailable` injected always; the read fails and the demo reads
  `err.diagnostics()` (on this multi-region account this also shows real region-failover attempts).
- **C. Hedging / region race** — hedging enabled + a delay injected on the first read leg, so an
  alternate region can win; prints the `HedgeDiagnostics` terminal state.

If the `COSMOSDB_MULTI_REGION` account is **unreachable**, the demo prints a note and falls back to
the offline demo automatically.

### OFFLINE mode (no account needed)

Builds each scenario synthetically from the public capture API — always runnable for a presentation:

```bash
cargo run -p azure_data_cosmos_driver --example diagnostics_demo
```

Offline sections: (1) typical success, (2) retry 429→200, (3) error op, (4) hedged multi-region,
(5) gate modes `Off`/`Always`/`Threshold`, (6) the `.NET`-style top-level `summary` block, and
(7) the `Json`/`Compact`/`Encoded` encoding modes with sizes.

### Live-demo narration (optional)

> "Every Cosmos operation produces a `DiagnosticsContext`. In LIVE mode we inject faults to force
> the interesting paths: a 429 that the driver retries to a 200, a 503 that triggers region
> failover, and a hedge race across regions — each with real server timings and activity ids, and a
> `FaultInjectionEvaluation` proving the fault fired. Every context carries the `.NET`-style summary
> roll-up and can be rendered as pretty JSON, compact JSON, or a base64 token."
