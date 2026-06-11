# Combo 4 — Deferred, threshold-gated capture (FINDINGS)

**Stack:** A3 hybrid · append-only pooled capture · C3 outcome+latency gate · D2/D3 build-on-demand.
**Role:** the design distilled from the team discussion — the recommended **lead** direction. It is
the synthesis the earlier "Combo 4 stretch" pointed at, now built and benchmarked.

## What it is

Three phases, with the hot path paying almost nothing:

1. **Hot path — write-preferred, compact, append-only.** Each operation rents a `Vec<u8>` from a
   `LogPool` and appends a raw TLV record stream (`CaptureLog`). Attribute keys are **implicit in the
   fixed record layout** (no key bytes at all), numbers are LEB128 varints, and the SDK/driver
   version + User-Agent provenance is a **single byte** referencing a process-global `Preamble`.
   Nothing is formatted; the only allocation is the pooled buffer growing.
2. **Gate — decide at `op_end`.** A `DiagnosticsPolicy { mode, latency_threshold, capture_on_error,
   binary }` is evaluated once when the outcome and elapsed time are known. If we don't want the
   diagnostics (fast success), the buffer is returned to the pool — **~free**.
3. **Build — only past the gate.** The raw log is parsed and rendered to a compact summary, and —
   opt-in (`policy.binary`) — to the shared `AZD1` binary blob (decoded by the same `diag-decode`
   tool as Combo 1/2). We pay the string-building cost only when we already know we want the output.

## Why it's the best of the prototypes

It takes the cheap capture + free drop from the **Binary Span Tree** and the aggregatable summary +
threshold tiering from the **Tiered Hybrid**, and pushes both further:

- **Capture is cheaper than the arena.** No `Node` structs, no per-attribute `String` keys — just
  appends of tags + varints + the one genuinely-dynamic string (the service request id). The drop
  path is a pooled `clear()`, not a `Vec<Node>` (with owned `String`s) drop.
- **The gate gains a latency dimension.** Earlier combos gated on success/error only; here a slow
  *success* (e.g. a 7 ms op over a 5 ms threshold) also builds, matching how `DiagnosticsThresholds`
  already works in the Cosmos driver.

## Benchmarks (median, release, Windows x86_64)

Happy-path (S1, fast success) — the cost you pay on the overwhelming majority of calls:

| Design | Happy-path cost (ns) |
|---|--:|
| Eager JSON Baseline | ~11,600 |
| Binary Span Tree (drop on success) | ~2,500 |
| Tiered Hybrid (summary default) | ~900 |
| **Combo 4 (gated, dropped)** | **~120** |

That ~120 ns is `collect` (append, ~50 ns) + return-to-pool (~70 ns) and **builds nothing** — roughly
**8× cheaper than the next-best design and ~95× cheaper than the eager baseline.** When the gate
*does* fire (S2/S3/S4), building a summary adds ~2–6 µs and the optional binary adds the
parse+encode (DEFLATE on wide fan-out), exactly as the other combos — but we're in the
"we-know-we-want-it" case. Full data: `DIAGNOSTICS-BENCH.csv` (`combo4` rows: `dropped`/`summary`/`detailed`).

## The version/User-Agent compaction trick (the ".NET" ask)

The SDK/driver version and UA suffix are constant per process, so they are **never stored
per-operation**: the hot path appends a single `PREAMBLE_ID` byte, and the full
`azsdk-rust-azure_data_cosmos/0.1.0 (windows; x86_64)` string is **rehydrated only at build time**
into both the summary's `client` block and the detailed tree's root attrs. Versions are packed as
`[major, minor, patch]` rather than ASCII. This mirrors .NET (Azure.Core builds the UA once; Cosmos
records it once in the summary, not per request).

## Honest caveats

- **RU is stored as `f32`** on the hot path (4 bytes, compact). Summing widens to `f64` and leaks
  precision noise (`8.4` → `8.39999962`), so the summary rounds RU on the way out. If exact RU
  fidelity matters, store `f64` (8 bytes) — a size/precision knob.
- **`collect` deltas are noisy** in the bench because they're computed as `drop-cycle − pool-noop`;
  the robust number is the **dropped total** (~120 ns at S1) and the per-record growth visible at
  S4 (more children → bigger append).
- **Async threading** isn't modeled here beyond the mock driver: in a real pipeline the `CaptureLog`
  must be threaded through the operation future as `&mut` (or parked in `Context`) to avoid a
  hot-path `Mutex`. This is the main open implementation question.
- **Self-contained blobs:** the preamble is embedded per built blob (rehydrated into root attrs) so
  the binary decodes standalone with the existing tool. A client-global symbol table would be
  smaller but make blobs non-standalone — a cross-SDK portability trade-off.

## Verdict

Strongest overall: the cheapest happy path by far, the same aggregatable summary + full binary
detail on demand as the Tiered Hybrid, and the compact version provenance the team asked for. The
real follow-ups are the async `&mut`/`Context` threading and the symbol-table sharing decision.
