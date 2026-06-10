# Combo 1 — Span + Encoded binary (FINDINGS)

**Stack:** A2/A3 span tree · C2 arena storage · D2 binary blob · D4 decode tool + skill.
**Role:** primary candidate. The full-fidelity, cheap-to-build, free-to-drop, small-on-the-wire design.

## What it is

The span tree lives in an **arena**: a flat `Vec<Node>` where each node stores its `parent: Option<u32>`,
kind, ticks, status, and attributes. Adding a span is a `Vec::push`; "passing a span" is a `u32`
index — no `Rc`/`RefCell`, no per-node allocation churn. On success the arena is **dropped without
serializing**; on error (or when verbose) it is projected to the shared `WireTree` and encoded to
the compact `AZD1` binary format. A bundled `diag-decode` binary turns a blob back into JSON.

## Arena ergonomics

- **Building is trivial and fast.** Push-and-return-id is about the cheapest tree you can build,
  and it is cache-friendly (one contiguous `Vec`).
- **The classic arena caveat — walking/serializing an index-linked tree — is real but sidestepped.**
  Turning an arena into a *nested* JSON tree is annoying (you chase indices and rebuild hierarchy).
  We avoid it entirely: the binary format is itself a **flat node list with index links**, so
  encoding is a straight loop over the `Vec` with no tree reconstruction. The hierarchy is only
  rebuilt by the *decoder*, off the hot path, when a human/agent actually needs to read it.

## Outcome-aware drop (the cost story)

`capture_blob(..., verbose=false)` returns `None` on success — the arena is dropped and **nothing
is serialized**. The happy path therefore pays only the arena pushes (collect). Construct +
serialize are paid solely on error or when explicitly requested. Dropping a `Vec<Node>` is
effectively free (measured in `DIAGNOSTICS-BENCH.csv` as `discard_ns`).

## Binary format & size wins

`AZD1` = `magic(4) + version(1) + flags(1) + payload`, payload = operation string, node count,
then per node: parent varint, kind byte, start/duration/status varints, and length-prefixed
attribute strings. LEB128 varints keep ticks and counts tiny. Compared to Combo 3's JSON, the
blob is dramatically smaller across all scenarios (see the bench table for the per-scenario
reduction multipliers), and the gap widens with fan-out (S4) because JSON repeats every quoted key.

## Compression trade-off

`encode_auto` DEFLATEs only when the uncompressed payload exceeds a threshold (256 B), so small
blobs (S1–S3) skip compression entirely (no CPU cost) while large fan-out blobs (S4) compress. The
flag byte records which path was taken so the decoder auto-detects. The threshold is the single
tuning knob; lower = smaller/heavier-CPU, higher = larger/cheaper.

## Version-header design

The 4-byte magic + version byte make the format self-describing and forward-evolvable: a decoder
checks the magic, rejects unknown versions cleanly (`DecodeError::UnsupportedVersion`), and a v2
can add fields behind a higher version without breaking v1 readers. This is the natural seam for a
cross-SDK wire contract.

## Decode tool + skill (D4)

`diag-decode <blob>` (or stdin) prints pretty JSON. It is the *real* decoder used to produce the
"decoded" samples in the report — see `SKILL.md` for the agent-facing skill description.

## Verdict

Strong primary candidate when full fidelity matters: cheapest happy path of the three (drop on
success), smallest wire size, and a clean version-headed contract. Cost: the bytes are opaque
without the tool, so the decode tool/skill must ship and be owned cross-SDK.
