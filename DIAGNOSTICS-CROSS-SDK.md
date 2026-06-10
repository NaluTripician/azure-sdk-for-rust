# Cross-SDK feasibility

> **Question:** what would shipping the recommended diagnostics design to .NET V3 / Java / Python /
> Go V4 look like, given where those SDKs are today?

## TL;DR

The valuable, hard-won part of this work — the **data contract** (span schema, attribute names,
binary wire format + version header, and the decode tool/skill) — is fully portable. The
**mechanisms** (Rust arena storage, the `tracing` crate) are language-local implementation details
each SDK already has an idiomatic equivalent for. So the cross-SDK cost is dominated by *agreeing
the contract*, not by re-implementing capture.

## 1. Portable vs. local

| Travels across SDKs (ratify once) | Rust-local (re-implement per language) |
|---|---|
| **Span schema** — operation → attempt(+transport) → routing child hierarchy; node kinds. | **Storage strategy** — Rust uses a `Vec`-backed arena with `u32` index links. |
| **Attribute names** — `az.service_request_id`, `az.request_charge`, `az.status_code`, `az.error_kind`, `az.plan_node_id`, `az.feed_range`, `az.attempt_count`, `az.outcome`, `az.endpoint`. | **Capture mechanism** — Rust uses the `tracing` crate + a subscriber layer (Combo 3) / wrapper types (Combo 2). |
| **Binary wire format** — `AZD1` magic + version byte + flags + flat node list with varints + optional DEFLATE. | **Concurrency/ownership plumbing** — borrow rules, `Arc<Mutex<_>>` for the capture store, etc. |
| **The summary schema** — the aggregatable request-style fields and the threshold rules. | **Threshold wiring** — how `SummaryThresholds` is plumbed through options. |
| **The decode tool + skill** — one `diag-decode` and its `SKILL.md`, shared by every SDK and by support tooling. | — |

The decoder reads the wire format, so **one** decode tool serves blobs produced by any SDK. That is
the single biggest portability win: producers can differ wildly, the consumer is shared.

## 2. Per-SDK mapping table

For each design axis, the idiomatic mechanism per SDK and the porting risk (Low / Med / High).

### Axis 2 — capture mechanism

| Option | Rust | .NET V3 | Java | Python | Go V4 |
|---|---|---|---|---|---|
| Bespoke types (B1) | structs | POCOs — **Low** | POJOs — **Low** | dataclasses — **Low** | structs — **Low** |
| Tracing-native (B2) | `tracing` + layer | `System.Diagnostics.Activity`/`ActivitySource` — **Low** (native) | OpenTelemetry API / Micrometer — **Low** | OpenTelemetry SDK / `logging` — **Med** | `log/slog` + otel — **Med** |
| Wrapper types (B3) | wrapper sink | wrapper around `Activity` + model — **Low** | wrapper — **Low** | wrapper — **Med** | wrapper — **Med** |

### Axis 3 — storage strategy

| Option | Rust | .NET V3 | Java | Python | Go V4 |
|---|---|---|---|---|---|
| Eager objects (C1) | nested structs | object graph — **Low** | object graph — **Low** | objects/dicts — **Low** | structs — **Low** |
| Arena (C2) | `Vec<Node>` + `u32` ids | `List<Node>` + int ids — **Low** | `ArrayList` + int ids — **Low** | list + int ids — **Low** | slice + int ids — **Low** |
| Outcome-aware (C3) | drop on success | conditional build — **Low** | conditional build — **Low** | conditional build — **Low** | conditional build — **Low** |

Arena storage ports cleanly: every language has a growable array and integer indices; none need
Rust's borrow checker to benefit from the flat layout.

### Axis 4 — output format

| Option | Rust | .NET V3 | Java | Python | Go V4 |
|---|---|---|---|---|---|
| JSON (D1) | `serde_json` | `System.Text.Json` — **Low** | Jackson — **Low** | `json` — **Low** | `encoding/json` — **Low** |
| Binary `AZD1` (D2) | hand-rolled varints + `flate2` | `BinaryWriter` + `DeflateStream` — **Low/Med** | `DataOutputStream` + `Deflater` — **Low/Med** | `struct`/`zlib` — **Med** | `encoding/binary` + `compress/flate` — **Low/Med** |
| Tiered (D3) | summary + detail | same two-tier — **Low** | same — **Low** | same — **Med** | same — **Low** |
| Decode tool + skill (D4) | `diag-decode` | **shared** — reuse the one tool — **Low** | shared — **Low** | shared — **Low** | shared — **Low** |

The only non-trivial porting item is the **binary encoder** (D2): each SDK must emit byte-identical
`AZD1`. That is a focused, test-vector-driven task (ship a few canonical blobs as golden files and
make every SDK match), not open-ended work.

## 3. .NET as the nearest analog

.NET V3 already ships **`CosmosDiagnostics`** — a request-focused, aggregatable diagnostics object
very close in spirit to Combo 2's *summary* view. Combined with Fabian's dedup/aggregation work
(rolling up repeated request diagnostics), .NET is the natural reference and **second adopter**:

- The summary schema should be validated against what `CosmosDiagnostics` already exposes, so the
  migration is "rename/realign fields" rather than "invent a new shape".
- .NET's `Activity`/`ActivitySource` gives B2 for free, so a .NET Combo 3 equivalent is essentially
  already present — useful as the OTel-export path there too.
- The dedup/aggregation logic is exactly the kind of rule that belongs in the shared **summary
  reducer** specification (see `DIAGNOSTICS-SIGNALS.md`).

## 4. Opt-in "mode" rollout

Ship without breaking anyone:

1. Every SDK keeps its **current** diagnostics as the default.
2. Add the new format behind an explicit flag / verbosity mode (Rust: `Verbosity::{Summary,Detailed}`).
3. Stabilize the schema + wire format while it is opt-in and low-stakes.
4. Once the contract is ratified and a couple of SDKs (Rust + .NET) have shipped it, flip the default
   per-SDK on its own schedule.

This lets the binary format and attribute names settle via real usage before they become a
compatibility surface.

## 5. Ratification list (must agree cross-SDK)

These are the items that **must** be agreed before two SDKs can share tooling:

1. **Service-request-id attribute key.** Rust core currently emits `az.service_request.id`; the
   spikes use `az.service_request_id`. Pick one canonical key.
2. **Request-charge (RU) attribute key.** e.g. `az.request_charge` (and whether RU is generic-core
   or Cosmos-only).
3. **Binary wire format + version policy.** The `AZD1` magic, the field order, varint encoding,
   the compression flag, and how versions evolve (the version byte + "reject unknown version" rule).
   Ship golden test vectors.
4. **Decode-tool ownership.** Who owns `diag-decode` (and the agent `SKILL.md`), where it lives, and
   how it is distributed so support and every SDK use the same decoder.
5. **Summary schema + reducer rules.** The always-present fields and the threshold-gated ones (seed
   from `DIAGNOSTICS-SIGNALS.md`; align with .NET `CosmosDiagnostics`).
6. **Node kinds & hierarchy.** The set of span kinds (operation/attempt/transport/routing/query) and
   the parent/child shape, so a decoded tree means the same thing everywhere.
