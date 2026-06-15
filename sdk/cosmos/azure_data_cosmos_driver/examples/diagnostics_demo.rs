// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License.

//! # Cosmos driver diagnostics — runnable demo
//!
//! A fully **offline** walk-through of the driver's gated diagnostics engine
//! (`azure_data_cosmos_driver::diagnostics::capture`). It builds each scenario from the public
//! capture API (the same front-end the driver uses on the hot path) and pretty-prints the resulting
//! canonical [`DiagnosticsContext`] — no live Cosmos account required.
//!
//! Run it:
//!
//! ```text
//! cargo run -p azure_data_cosmos_driver --example diagnostics_demo
//! ```
//!
//! ## What each section shows
//!
//! 1. **Typical success** — a single-attempt `200`: activity id, status, request charge, region,
//!    endpoint, and per-attempt server timing.
//! 2. **Retry (429 → 200)** — two `RequestDiagnostics` with `ExecutionContext::Initial` then
//!    `Retry`, including the throttle sub-status `3200` on the first attempt.
//! 3. **Error operation** — a terminal failure: final status + sub-status and the service request
//!    (activity) id captured from the response.
//! 4. **Hedged multi-region** — the per-region legs, which leg won, and the `HedgeDiagnostics`
//!    terminal state.
//! 5. **Gate modes side by side** — the same operation under `Off`, `Always`, and `Threshold`, so
//!    the cost/visibility trade is obvious (including a `Threshold` fast-success that is dropped).
//! 6. **Summary block** — the `.NET CosmosDiagnostics`-style top-level `summary` (computed at
//!    finalization): status histogram, retry/throttle counts, regions, final status, total RU.
//! 7. **Encoding modes** — the same context rendered `Json` / `Compact` / `Encoded` (a
//!    `DriverOptions` client option), with sizes.
//!
//! The canonical JSON for each context exposes every rich field slot (`events`, transport-shard,
//! `fault_injection_evaluations`, sub-status, …). The synthetic offline scenarios populate the
//! operation/attempt-level fields; `events` and transport-shard detail are filled by the live
//! pipeline and appear here as their empty slots.

use azure_data_cosmos_driver::diagnostics::capture::{
    finish, should_build, AttemptRecord, DiagnosticsPolicy, DiagnosticsRecorder, HedgeOutcome,
    LogPool, Outcome,
};
use azure_data_cosmos_driver::diagnostics::{DiagnosticsContext, ExecutionContext};
use azure_data_cosmos_driver::options::DiagnosticsOptions;
use azure_data_cosmos_driver::{DiagnosticsEncoding, DiagnosticsVerbosity};
use std::sync::Arc;
use std::time::Duration;

fn options() -> Arc<DiagnosticsOptions> {
    Arc::new(DiagnosticsOptions::default())
}

/// Prints a section header + a one-line caption explaining what it demonstrates.
fn header(title: &str, caption: &str) {
    println!("\n{}", "=".repeat(96));
    println!("| {title}");
    println!("| {caption}");
    println!("{}", "=".repeat(96));
}

/// Pretty-prints the canonical detailed JSON for a context (round-tripped through `serde_json`).
fn print_pretty_json(ctx: &DiagnosticsContext) {
    let compact = ctx.to_json_string(Some(DiagnosticsVerbosity::Detailed));
    match serde_json::from_str::<serde_json::Value>(compact)
        .and_then(|v| serde_json::to_string_pretty(&v))
    {
        Ok(pretty) => println!("{pretty}"),
        Err(_) => println!("{compact}"),
    }
}

/// Prints a compact, human-readable summary of every `RequestDiagnostics` in a context.
fn summarize_attempts(ctx: &DiagnosticsContext) {
    println!("  activity_id      : {}", ctx.activity_id().as_str());
    println!(
        "  operation status : {}",
        ctx.status()
            .map(|s| s.to_string())
            .unwrap_or_else(|| "<none>".to_string())
    );
    println!(
        "  total RU charge  : {:.2}",
        ctx.total_request_charge().value()
    );
    println!("  request count    : {}", ctx.request_count());
    let regions: Vec<String> = ctx
        .regions_contacted()
        .iter()
        .map(|r| r.as_str().to_string())
        .collect();
    println!("  regions contacted: {regions:?}");
    for (i, req) in ctx.requests().iter().enumerate() {
        let region = req.region().map(|r| r.as_str()).unwrap_or("-");
        let server_ms = req
            .server_duration_ms()
            .map(|ms| format!("{ms:.1}ms"))
            .unwrap_or_else(|| "-".to_string());
        let svc_id = req.activity_id().map(|a| a.as_str()).unwrap_or("-");
        let error = req
            .error()
            .map(|e| format!(" error=\"{e}\""))
            .unwrap_or_default();
        println!(
            "  [attempt {i}] ctx={:<9} region={:<8} status={:<24} RU={:.2} server={} svc_id={} sent={:?}{}",
            format!("{:?}", req.execution_context()),
            region,
            req.status().to_string(),
            req.request_charge().value(),
            server_ms,
            svc_id,
            req.request_sent(),
            error,
        );
    }
}

/// Records a typical single-attempt success into a fresh recorder.
fn record_typical(pool: &LogPool) -> DiagnosticsRecorder {
    let mut rec = DiagnosticsRecorder::start(
        pool,
        "read_item",
        "https://acct.documents.azure.com/",
        "11111111-aaaa-bbbb-cccc-typical00single",
    );
    rec.record_attempt(
        AttemptRecord::new(
            ExecutionContext::Initial,
            "East US",
            "https://acct-eastus.documents.azure.com/",
            200,
        )
        .with_service_request_id("req-eastus-ok")
        .with_request_charge(2.89)
        .with_duration_ns(3_900_000),
    );
    rec.record_end(Outcome::Success, 1, 200, None, Some(4_100_000));
    rec
}

/// Records a throttled-then-retried success (429 -> 200) into a fresh recorder.
fn record_retry(pool: &LogPool) -> DiagnosticsRecorder {
    let mut rec = DiagnosticsRecorder::start(
        pool,
        "create_item",
        "https://acct.documents.azure.com/",
        "22222222-aaaa-bbbb-cccc-retry000429200",
    );
    rec.record_attempt(
        AttemptRecord::new(
            ExecutionContext::Initial,
            "East US",
            "https://acct-eastus.documents.azure.com/",
            429,
        )
        .with_service_request_id("req-eastus-429")
        .with_request_charge(4.2)
        .with_sub_status(3200)
        .with_duration_ns(3_200_000),
    );
    rec.record_attempt(
        AttemptRecord::new(
            ExecutionContext::Retry,
            "East US",
            "https://acct-eastus.documents.azure.com/",
            200,
        )
        .with_service_request_id("req-eastus-200")
        .with_request_charge(4.2)
        .with_duration_ns(4_100_000),
    );
    rec.record_end(Outcome::Success, 2, 200, None, Some(7_300_000));
    rec
}

/// Records a terminal error operation (503 / sub-status) into a fresh recorder.
fn record_error(pool: &LogPool) -> DiagnosticsRecorder {
    let mut rec = DiagnosticsRecorder::start(
        pool,
        "read_item",
        "https://acct.documents.azure.com/",
        "33333333-aaaa-bbbb-cccc-error000503xx",
    );
    rec.record_attempt(
        AttemptRecord::new(
            ExecutionContext::Initial,
            "East US",
            "https://acct-eastus.documents.azure.com/",
            503,
        )
        .with_service_request_id("req-eastus-503")
        .with_sub_status(21005)
        .with_request_sent("sent")
        .with_duration_ns(6_700_000),
    );
    rec.record_end(Outcome::Error, 1, 503, Some(21005), Some(7_000_000));
    rec
}

/// Records a hedged multi-region operation (alternate region wins) into a fresh recorder.
fn record_hedged(pool: &LogPool) -> DiagnosticsRecorder {
    let mut rec = DiagnosticsRecorder::start(
        pool,
        "read_item",
        "https://acct.documents.azure.com/",
        "44444444-aaaa-bbbb-cccc-hedged00multi",
    );
    // Primary leg (East US) is slow and never responds before the alternate wins.
    rec.record_attempt(
        AttemptRecord::new(
            ExecutionContext::Hedging,
            "East US",
            "https://acct-eastus.documents.azure.com/",
            0,
        )
        .with_request_sent("sent")
        .with_duration_ns(8_500_000),
    );
    // Alternate leg (West US) returns first and wins the race.
    rec.record_attempt(
        AttemptRecord::new(
            ExecutionContext::Hedging,
            "West US",
            "https://acct-westus.documents.azure.com/",
            200,
        )
        .with_service_request_id("req-westus-200")
        .with_request_charge(3.1)
        .with_duration_ns(4_300_000),
    );
    rec.record_hedge_outcome(
        HedgeOutcome::AlternateWon,
        Duration::from_millis(500),
        "East US",
        Some("West US"),
        Some("West US"),
    );
    rec.record_end(Outcome::Success, 2, 200, None, Some(9_000_000));
    rec
}

/// A slow single-attempt success (8 ms) used for the gate-mode comparison.
fn record_slow_success(pool: &LogPool) -> DiagnosticsRecorder {
    let mut rec = DiagnosticsRecorder::start(
        pool,
        "read_item",
        "https://acct.documents.azure.com/",
        "55555555-aaaa-bbbb-cccc-gate0000slow",
    );
    rec.record_attempt(
        AttemptRecord::new(
            ExecutionContext::Initial,
            "East US",
            "https://acct-eastus.documents.azure.com/",
            200,
        )
        .with_service_request_id("req-eastus-slow")
        .with_request_charge(2.5)
        .with_duration_ns(8_000_000),
    );
    rec.record_end(Outcome::Success, 1, 200, None, Some(8_000_000));
    rec
}

/// A fast single-attempt success (1 ms) used to show a `Threshold` drop.
fn record_fast_success(pool: &LogPool) -> DiagnosticsRecorder {
    let mut rec = DiagnosticsRecorder::start(
        pool,
        "read_item",
        "https://acct.documents.azure.com/",
        "66666666-aaaa-bbbb-cccc-gate0000fast",
    );
    rec.record_attempt(
        AttemptRecord::new(
            ExecutionContext::Initial,
            "East US",
            "https://acct-eastus.documents.azure.com/",
            200,
        )
        .with_service_request_id("req-eastus-fast")
        .with_request_charge(2.5)
        .with_duration_ns(1_000_000),
    );
    rec.record_end(Outcome::Success, 1, 200, None, Some(1_000_000));
    rec
}

/// One-line description of a gated result for the side-by-side comparison.
fn describe(result: &Option<DiagnosticsContext>) -> String {
    match result {
        None => "DROPPED - no diagnostics produced (None)".to_string(),
        Some(ctx) => format!(
            "BUILT - DiagnosticsContext {{ requests: {}, status: {}, RU: {:.2} }}",
            ctx.request_count(),
            ctx.status()
                .map(|s| s.to_string())
                .unwrap_or_else(|| "<none>".to_string()),
            ctx.total_request_charge().value(),
        ),
    }
}

fn main() {
    let pool = LogPool::new();

    println!("\nCosmos driver diagnostics - offline demo");
    println!("Engine: azure_data_cosmos_driver::diagnostics::capture (gated, lock-free hot path)");

    // -- Section 1: typical success ------------------------------------------------------------
    header(
        "1. Typical single-attempt success",
        "One request, 200 OK - the canonical DiagnosticsContext with activity id, status, RU, region, timing.",
    );
    let ctx = finish(
        record_typical(&pool),
        &DiagnosticsPolicy::always(),
        options(),
    )
    .expect("Always mode builds a context");
    summarize_attempts(&ctx);
    println!("\n  canonical JSON:");
    print_pretty_json(&ctx);

    // -- Section 2: retry (429 -> 200) ---------------------------------------------------------
    header(
        "2. Retry after throttling (429 -> 200)",
        "Two RequestDiagnostics: ExecutionContext::Initial (429/3200 RUBudgetExceeded) then Retry (200).",
    );
    let ctx = finish(record_retry(&pool), &DiagnosticsPolicy::always(), options())
        .expect("Always mode builds a context");
    summarize_attempts(&ctx);
    println!("\n  canonical JSON:");
    print_pretty_json(&ctx);

    // -- Section 3: error operation ------------------------------------------------------------
    header(
        "3. Error operation (503 / sub-status)",
        "A terminal failure - final status + sub-status and the captured service request id on the error path.",
    );
    let ctx = finish(record_error(&pool), &DiagnosticsPolicy::always(), options())
        .expect("Always mode builds a context");
    summarize_attempts(&ctx);
    println!("\n  canonical JSON:");
    print_pretty_json(&ctx);

    // -- Section 4: hedged multi-region --------------------------------------------------------
    header(
        "4. Hedged multi-region operation (alternate region wins)",
        "Two hedging legs (East US no-response, West US wins) + HedgeDiagnostics terminal state.",
    );
    let ctx = finish(
        record_hedged(&pool),
        &DiagnosticsPolicy::always(),
        options(),
    )
    .expect("Always mode builds a context");
    summarize_attempts(&ctx);
    if let Some(hedge) = ctx.hedge_diagnostics() {
        println!("\n  hedge race:");
        println!("    terminal_state  : {:?}", hedge.terminal_state());
        println!("    primary_region  : {}", hedge.primary_region().as_str());
        println!(
            "    alternate_region: {:?}",
            hedge.alternate_region().map(|r| r.as_str())
        );
        println!(
            "    winning_region  : {:?}",
            hedge.response_region().map(|r| r.as_str())
        );
    }
    println!("\n  canonical JSON:");
    print_pretty_json(&ctx);

    // -- Section 5: gate modes side by side ----------------------------------------------------
    header(
        "5. Gate modes side by side (Off / Always / Threshold)",
        "Same operation, three policies - the gate decides whether to build the context at op-end.",
    );

    let off = finish(
        record_slow_success(&pool),
        &DiagnosticsPolicy::off(),
        options(),
    );
    println!(
        "  Off       (slow 8ms success)        -> {}",
        describe(&off)
    );

    let always = finish(
        record_slow_success(&pool),
        &DiagnosticsPolicy::always(),
        options(),
    );
    println!(
        "  Always    (slow 8ms success)        -> {}",
        describe(&always)
    );

    let threshold = DiagnosticsPolicy::threshold(Duration::from_millis(5));
    let slow = finish(record_slow_success(&pool), &threshold, options());
    println!(
        "  Threshold (slow 8ms success, >5ms)  -> {}",
        describe(&slow)
    );

    let fast = finish(record_fast_success(&pool), &threshold, options());
    println!(
        "  Threshold (fast 1ms success, <5ms)  -> {}",
        describe(&fast)
    );

    println!("\n  gate predicate (should_build):");
    for (outcome, total_ns, policy) in [
        (Outcome::Success, 8_000_000, DiagnosticsPolicy::off()),
        (Outcome::Success, 1_000_000, DiagnosticsPolicy::always()),
        (Outcome::Success, 8_000_000, threshold),
        (Outcome::Success, 1_000_000, threshold),
        (Outcome::Error, 1_000_000, threshold),
    ] {
        println!(
            "    mode={:<9} outcome={:<7} total={:>2}ms -> build = {}",
            format!("{:?}", policy.mode),
            format!("{outcome:?}"),
            total_ns / 1_000_000,
            should_build(outcome, total_ns, &policy),
        );
    }

    // -- Section 6: .NET-style summary block ---------------------------------------------------
    header(
        "6. Top-level summary block (.NET CosmosDiagnostics-style)",
        "Computed once at finalization (after the requests) - a roll-up over the retry op from section 2.",
    );
    let ctx = finish(record_retry(&pool), &DiagnosticsPolicy::always(), options())
        .expect("Always mode builds a context");
    let summary = ctx.summary();
    println!("  request_count    : {}", summary.request_count());
    println!("  retry_count      : {}", summary.retry_count());
    println!("  throttled_count  : {}", summary.throttled_count());
    println!(
        "  total RU charge  : {:.2}",
        summary.total_request_charge().value()
    );
    println!("  regions          : {:?}", summary.regions_contacted());
    println!(
        "  final status     : {}",
        summary
            .final_status()
            .map(|s| s.to_string())
            .unwrap_or_else(|| "<none>".to_string())
    );
    println!("  top error        : {:?}", summary.top_error());
    println!("\n  summary JSON (emitted at the top of the context output):");
    match serde_json::to_value(summary).and_then(|v| serde_json::to_string_pretty(&v)) {
        Ok(pretty) => println!("{pretty}"),
        Err(_) => println!("  <unavailable>"),
    }

    // -- Section 7: encoding modes side by side ------------------------------------------------
    header(
        "7. Diagnostics encoding modes (Json / Compact / Encoded)",
        "Same context rendered three ways via DiagnosticsContext::encode - a DriverOptions client option.",
    );
    let json = ctx.encode(DiagnosticsEncoding::Json);
    let compact = ctx.encode(DiagnosticsEncoding::Compact);
    let encoded = ctx.encode(DiagnosticsEncoding::Encoded);
    println!("  Json    (default, pretty)  : {} bytes", json.len());
    println!("  Compact (minified JSON)    : {} bytes", compact.len());
    println!(
        "  Encoded (base64 of compact): {} bytes  (decodes back to the compact JSON)",
        encoded.len()
    );
    println!("\n  Compact:\n{compact}");
    println!("\n  Encoded:\n{encoded}");

    println!("\n{}", "=".repeat(96));
    println!("Done. Default policy is Mode::Always (diagnostics out-of-the-box); Off disables the");
    println!(
        "per-request build on the hot path; Threshold surfaces only on slow/errored operations."
    );
    println!("The summary block is computed at finalization; encoding is a DriverOptions option.");
    println!("{}", "=".repeat(96));
}
