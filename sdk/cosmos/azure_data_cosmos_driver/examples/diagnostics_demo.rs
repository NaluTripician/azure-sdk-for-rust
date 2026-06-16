// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License.

//! # Cosmos driver diagnostics — runnable demo (LIVE or offline)
//!
//! A walk-through of the driver's gated diagnostics engine
//! (`azure_data_cosmos_driver::diagnostics::capture`), in two modes:
//!
//! - **LIVE** (preferred) — connects to a real Cosmos account and uses **fault injection** to force
//!   429-throttle-then-retry, a 503 server error, and a hedging/region race, so the printed
//!   [`DiagnosticsContext`] carries **real** server timings, activity ids, and regions. Requires the
//!   `reqwest` + `fault_injection` features and a reachable account.
//! - **OFFLINE** (fallback) — builds each scenario synthetically from the public capture API, so the
//!   demo always runs for a presentation even with no account.
//!
//! Run it LIVE (uses the `COSMOS_CONNECTION_STRING` account **only** — tries master-key auth from
//! the connection string, then Entra ID for the same endpoint; secret values are never printed;
//! creates and **deletes** a temp database):
//!
//! ```text
//! cargo run -p azure_data_cosmos_driver --example diagnostics_demo --features "reqwest fault_injection"
//! ```
//!
//! Run it OFFLINE (no features, no account needed):
//!
//! ```text
//! cargo run -p azure_data_cosmos_driver --example diagnostics_demo
//! ```
//!
//! When built with the live features but no account is reachable, it prints a note and falls back to
//! the offline demo automatically.
//!
//! ## What the OFFLINE sections show
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
//! The LIVE run shows the same diagnostics — Summary, encoding modes, and `fault_injection_evaluations`
//! confirming each injected fault fired — but with real timings and region names from the service.

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
    // Prefer a LIVE run (real Cosmos account + fault injection → real timings, activity ids,
    // regions). Requires the `reqwest` + `fault_injection` features. Falls back to the offline
    // synthetic demo when those features are off or no account is reachable — so the demo always
    // runs for a presentation.
    #[cfg(all(feature = "reqwest", feature = "fault_injection"))]
    {
        match live::try_run_live() {
            live::LiveOutcome::Ran => return,
            live::LiveOutcome::FellBack(reason) => {
                println!("\n{}", "#".repeat(96));
                println!("# LIVE mode unavailable ({reason}).");
                println!("# Falling back to the OFFLINE synthetic demo.");
                println!("{}", "#".repeat(96));
            }
        }
    }
    #[cfg(not(all(feature = "reqwest", feature = "fault_injection")))]
    {
        println!("\n{}", "#".repeat(96));
        println!("# Built without the `reqwest` + `fault_injection` features — running OFFLINE.");
        println!("# For the LIVE demo (real account + fault injection), run:");
        println!("#   cargo run -p azure_data_cosmos_driver --example diagnostics_demo \\");
        println!("#     --features \"reqwest fault_injection\"");
        println!("{}", "#".repeat(96));
    }
    run_offline();
}

/// The offline, fully synthetic demo (no network). Always runnable; used as the live fallback.
fn run_offline() {
    let pool = LogPool::new();

    println!("\nCosmos driver diagnostics - OFFLINE demo (synthetic scenarios)");
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

/// Live demo against a real Cosmos account, using fault injection to force the scenarios so the
/// captured diagnostics carry **real** timings, activity ids, and regions. Compiled only with the
/// `reqwest` + `fault_injection` features; falls back to the offline demo when no account is
/// reachable. Never prints secret values (only the endpoint host).
#[cfg(all(feature = "reqwest", feature = "fault_injection"))]
mod live {
    use azure_data_cosmos_driver::diagnostics::capture::DiagnosticsPolicy;
    use azure_data_cosmos_driver::diagnostics::DiagnosticsContext;
    use azure_data_cosmos_driver::driver::{CosmosDriver, CosmosDriverRuntime};
    use azure_data_cosmos_driver::error::CosmosError;
    use azure_data_cosmos_driver::fault_injection::{
        FaultInjectionConditionBuilder, FaultInjectionErrorType, FaultInjectionEvaluation,
        FaultInjectionResultBuilder, FaultInjectionRule, FaultInjectionRuleBuilder,
        FaultOperationType,
    };
    use azure_data_cosmos_driver::models::{
        AccountReference, ConnectionString, ContainerReference, CosmosOperation, CosmosResponse,
        DatabaseReference, ItemReference, PartitionKey,
    };
    use azure_data_cosmos_driver::options::{
        AvailabilityStrategy, DriverOptions, HedgeThreshold, HedgingStrategy, OperationOptions,
        OperationOptionsBuilder,
    };
    use azure_data_cosmos_driver::DiagnosticsEncoding;
    use std::sync::Arc;
    use std::time::Duration;
    use url::Url;

    /// The ONLY account this demo uses. (Per the constraint, `COSMOS_TEST61` and
    /// `COSMOSDB_MULTI_REGION` are not used.)
    const ACCOUNT_VAR: &str = "COSMOS_CONNECTION_STRING";

    const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
    const OP_TIMEOUT: Duration = Duration::from_secs(30);

    pub enum LiveOutcome {
        Ran,
        FellBack(&'static str),
    }

    /// Entry point from `main`. Builds a Tokio runtime and runs the live demo.
    pub fn try_run_live() -> LiveOutcome {
        let rt = match tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(_) => return LiveOutcome::FellBack("could not start async runtime"),
        };
        rt.block_on(run_live_async())
    }

    /// A connected account: its host + auth method + reference (key/token never printed).
    struct Connected {
        host: String,
        auth: &'static str,
        account: AccountReference,
    }

    /// Connects to the `COSMOS_CONNECTION_STRING` account (only). Tries master-key auth first
    /// (from the connection string), then Entra ID via the developer-tools credential — the account
    /// may have local/master-key auth disabled. Returns the first that initializes (a real init
    /// round-trip). Never prints the key or token.
    async fn connect() -> Option<Connected> {
        let var = ACCOUNT_VAR;
        let Ok(raw) = std::env::var(var) else {
            println!("  [{var}] absent");
            return None;
        };
        let Ok(conn) = raw.parse::<ConnectionString>() else {
            println!("  [{var}] connection string did not parse");
            return None;
        };
        let Ok(endpoint) = Url::parse(conn.account_endpoint()) else {
            println!("  [{var}] endpoint is not a valid URL");
            return None;
        };
        let host = endpoint.host_str().unwrap_or("<unknown>").to_string();

        // Candidate credentials for the SAME endpoint: master key, then Entra ID.
        let mut candidates: Vec<(&'static str, AccountReference)> = vec![(
            "master-key",
            AccountReference::with_master_key(endpoint.clone(), conn.account_key().clone()),
        )];
        match azure_identity::DeveloperToolsCredential::new(None) {
            Ok(cred) => candidates.push((
                "entra (developer tools)",
                AccountReference::with_credential(endpoint.clone(), cred),
            )),
            Err(e) => println!("  [{var}] could not build Entra credential: {e}"),
        }

        for (auth, account) in candidates {
            let Ok(runtime) = CosmosDriverRuntime::builder().build().await else {
                println!("  [{var}] ({host}) runtime build failed [{auth}]");
                continue;
            };
            let opts = DriverOptions::builder(account.clone())
                .with_capture_diagnostics_policy(DiagnosticsPolicy::always())
                .build();
            match tokio::time::timeout(
                CONNECT_TIMEOUT,
                runtime.get_or_create_driver(account.clone(), Some(opts)),
            )
            .await
            {
                Ok(Ok(_driver)) => {
                    println!("  [{var}] ({host}) connected via {auth} ✓");
                    return Some(Connected {
                        host,
                        auth,
                        account,
                    });
                }
                Ok(Err(e)) => println!(
                    "  [{var}] ({host}) {auth} init failed: {} — trying next",
                    e.status()
                ),
                Err(_) => println!("  [{var}] ({host}) {auth} timed out — trying next"),
            }
        }
        None
    }

    async fn run_live_async() -> LiveOutcome {
        println!("\nCosmos driver diagnostics - LIVE demo (real account + fault injection)");
        println!("Account: COSMOS_CONNECTION_STRING only (secret values are never printed):");
        let Some(conn) = connect().await else {
            return LiveOutcome::FellBack("COSMOS_CONNECTION_STRING account unreachable");
        };

        println!("\n{}", "=".repeat(96));
        println!(
            "| Using account: COSMOS_CONNECTION_STRING (host {}, auth {})",
            conn.host, conn.auth
        );
        println!(
            "| Mode: LIVE — diagnostics below carry real server timings, activity ids, regions."
        );
        println!("{}", "=".repeat(96));

        // Unique temp resources, cleaned up at the end.
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let db_name = format!("diag-demo-{suffix}");
        let container_name = "items";

        let result = run_scenarios(&conn, &db_name, container_name).await;

        // Best-effort cleanup regardless of outcome.
        if let Err(e) = cleanup(&conn, &db_name).await {
            println!("\n[cleanup] could not delete temp database {db_name}: {e}");
        } else {
            println!("\n[cleanup] deleted temp database {db_name} ✓");
        }

        match result {
            Ok(()) => LiveOutcome::Ran,
            Err(e) => {
                println!("\n[live] scenario error: {e}");
                // We still connected and exercised the account, so count it as a live run.
                LiveOutcome::Ran
            }
        }
    }

    async fn run_scenarios(
        conn: &Connected,
        db_name: &str,
        container_name: &str,
    ) -> Result<(), CosmosError> {
        // --- Setup: create DB + container + seed an item (no fault injection) ---
        super::header(
            "Setup: create a temp database + container + seed item (no faults)",
            "Plain runtime; these resources are deleted at the end of the demo.",
        );
        let plain = runtime_with_rules(Vec::new()).await?;
        let driver = driver_for(&plain, &conn.account).await?;
        op_create_database(&driver, &conn.account, db_name).await?;
        let db_ref = DatabaseReference::from_name(conn.account.clone(), db_name.to_string());
        let container = op_create_container(&driver, &db_ref, container_name, "/pk").await?;
        let item_body = br#"{"id":"demo-item","pk":"demo-pk","value":"hello-live"}"#;
        op_create_item(&driver, &container, "demo-item", pk(), item_body).await?;
        println!("  created database={db_name} container={container_name}, seeded 1 item ✓");

        // --- Scenario A: 429 throttling then a successful retry ---
        super::header(
            "A. LIVE 429 throttling -> automatic retry -> success (fault-injected)",
            "Inject TooManyRequests on ReadItem with a hit-limit; the driver retries to a real 200.",
        );
        let rule_429 = Arc::new(
            FaultInjectionRuleBuilder::new(
                "demo-429",
                FaultInjectionResultBuilder::new()
                    .with_error(FaultInjectionErrorType::TooManyRequests)
                    .with_probability(1.0)
                    .build(),
            )
            .with_condition(
                FaultInjectionConditionBuilder::new()
                    .with_operation_type(FaultOperationType::ReadItem)
                    .build(),
            )
            .with_hit_limit(2)
            .build(),
        );
        match read_with_rules(conn, &container, vec![rule_429]).await {
            Ok(resp) => show_context("typical (429 then retry)", &resp.diagnostics()),
            Err(e) => show_error_context("429 scenario", &e),
        }

        // --- Scenario B: 503 server error ---
        super::header(
            "B. LIVE 503 server error (fault-injected, always)",
            "Inject ServiceUnavailable on ReadItem; the read fails and we read err.diagnostics().",
        );
        let rule_503 = Arc::new(
            FaultInjectionRuleBuilder::new(
                "demo-503",
                FaultInjectionResultBuilder::new()
                    .with_error(FaultInjectionErrorType::ServiceUnavailable)
                    .with_probability(1.0)
                    .build(),
            )
            .with_condition(
                FaultInjectionConditionBuilder::new()
                    .with_operation_type(FaultOperationType::ReadItem)
                    .build(),
            )
            .build(),
        );
        match read_with_rules(conn, &container, vec![rule_503]).await {
            Ok(resp) => {
                println!("  (unexpected success) ");
                show_context("503 scenario (unexpected ok)", &resp.diagnostics());
            }
            Err(e) => show_error_context("error (503)", &e),
        }

        // --- Scenario C: hedging / region race (best-effort, multi-region accounts only) ---
        super::header(
            "C. LIVE hedging / region race (best-effort; multi-region accounts)",
            "Enable hedging + inject a delay on the first read leg so an alternate region can win.",
        );
        run_hedging_scenario(conn, &container).await;

        Ok(())
    }

    /// Builds a runtime, optionally with fault-injection rules.
    async fn runtime_with_rules(
        rules: Vec<Arc<FaultInjectionRule>>,
    ) -> Result<Arc<CosmosDriverRuntime>, CosmosError> {
        let builder = CosmosDriverRuntime::builder();
        let builder = if rules.is_empty() {
            builder
        } else {
            builder.with_fault_injection_rules(rules)?
        };
        builder.build().await
    }

    async fn driver_for(
        runtime: &Arc<CosmosDriverRuntime>,
        account: &AccountReference,
    ) -> Result<Arc<CosmosDriver>, CosmosError> {
        let opts = DriverOptions::builder(account.clone())
            .with_capture_diagnostics_policy(DiagnosticsPolicy::always())
            .build();
        runtime
            .get_or_create_driver(account.clone(), Some(opts))
            .await
    }

    /// Reads the seeded item through a fresh runtime carrying the given fault rules.
    async fn read_with_rules(
        conn: &Connected,
        container: &ContainerReference,
        rules: Vec<Arc<FaultInjectionRule>>,
    ) -> Result<CosmosResponse, CosmosError> {
        let runtime = runtime_with_rules(rules).await?;
        let driver = driver_for(&runtime, &conn.account).await?;
        op_read_item(&driver, container, "demo-item", pk()).await
    }

    /// Best-effort hedging scenario. Reports gracefully if the account can't produce a hedge.
    async fn run_hedging_scenario(conn: &Connected, container: &ContainerReference) {
        let hedging = AvailabilityStrategy::Hedging(HedgingStrategy::new(
            HedgeThreshold::new(Duration::from_millis(100)).expect("100ms is a valid threshold"),
        ));
        let op_options = OperationOptionsBuilder::new()
            .with_availability_strategy(hedging)
            .build();
        let runtime = match runtime_with_rules(vec![delay_rule()]).await {
            Ok(r) => r,
            Err(e) => {
                println!("  hedging runtime build failed: {} — skipping", e.status());
                return;
            }
        };
        let driver = match driver_for(&runtime, &conn.account).await {
            Ok(d) => d,
            Err(e) => {
                println!("  hedging driver init failed: {} — skipping", e.status());
                return;
            }
        };
        let item = ItemReference::from_name(container, pk(), "demo-item".to_string());
        let op = CosmosOperation::read_item(item);
        match tokio::time::timeout(
            OP_TIMEOUT,
            driver.execute_singleton_operation(op, op_options),
        )
        .await
        {
            Ok(Ok(resp)) => {
                let ctx = resp.diagnostics();
                if let Some(h) = ctx.hedge_diagnostics() {
                    println!(
                        "  HEDGE fired: terminal_state={:?} primary={} winning={:?}",
                        h.terminal_state(),
                        h.primary_region().as_str(),
                        h.response_region().map(|r| r.as_str())
                    );
                } else {
                    println!(
                        "  No hedge recorded (single-region routing or the primary won pre-threshold)."
                    );
                }
                show_context("hedging read", &ctx);
            }
            Ok(Err(e)) => show_error_context("hedging read", &e),
            Err(_) => println!("  hedging read timed out — skipping"),
        }
    }

    /// A delay fault on the first read leg (used to nudge a hedge to an alternate region).
    fn delay_rule() -> Arc<FaultInjectionRule> {
        Arc::new(
            FaultInjectionRuleBuilder::new(
                "demo-hedge-delay",
                FaultInjectionResultBuilder::new()
                    .with_delay(Duration::from_secs(3))
                    .with_probability(1.0)
                    .build(),
            )
            .with_condition(
                FaultInjectionConditionBuilder::new()
                    .with_operation_type(FaultOperationType::ReadItem)
                    .build(),
            )
            .with_hit_limit(1)
            .build(),
        )
    }

    fn pk() -> PartitionKey {
        "demo-pk".to_string().into()
    }

    // ---- raw CosmosOperation helpers (mirroring the test framework) ----

    async fn op_create_database(
        driver: &CosmosDriver,
        account: &AccountReference,
        db_name: &str,
    ) -> Result<(), CosmosError> {
        let body = format!(r#"{{"id":"{db_name}"}}"#);
        let op = CosmosOperation::create_database(account.clone()).with_body(body.into_bytes());
        driver
            .execute_singleton_operation(op, OperationOptions::default())
            .await
            .map(|_| ())
    }

    async fn op_create_container(
        driver: &CosmosDriver,
        database: &DatabaseReference,
        name: &str,
        pk_path: &str,
    ) -> Result<ContainerReference, CosmosError> {
        let body = format!(
            r#"{{"id":"{name}","partitionKey":{{"paths":["{pk_path}"],"kind":"Hash","version":2}}}}"#
        );
        let op = CosmosOperation::create_container(database.clone()).with_body(body.into_bytes());
        driver
            .execute_singleton_operation(op, OperationOptions::default())
            .await?;
        let db_name = database.name().unwrap_or_default();
        driver.resolve_container_by_name(db_name, name).await
    }

    async fn op_create_item(
        driver: &CosmosDriver,
        container: &ContainerReference,
        item_id: &str,
        partition_key: PartitionKey,
        body: &[u8],
    ) -> Result<(), CosmosError> {
        let item = ItemReference::from_name(container, partition_key, item_id.to_string());
        let op = CosmosOperation::create_item(item).with_body(body.to_vec());
        driver
            .execute_singleton_operation(op, OperationOptions::default())
            .await
            .map(|_| ())
    }

    async fn op_read_item(
        driver: &CosmosDriver,
        container: &ContainerReference,
        item_id: &str,
        partition_key: PartitionKey,
    ) -> Result<CosmosResponse, CosmosError> {
        let item = ItemReference::from_name(container, partition_key, item_id.to_string());
        let op = CosmosOperation::read_item(item);
        driver
            .execute_singleton_operation(op, OperationOptions::default())
            .await
    }

    async fn cleanup(conn: &Connected, db_name: &str) -> Result<(), CosmosError> {
        let runtime = runtime_with_rules(Vec::new()).await?;
        let driver = driver_for(&runtime, &conn.account).await?;
        let db_ref = DatabaseReference::from_name(conn.account.clone(), db_name.to_string());
        let op = CosmosOperation::delete_database(db_ref);
        driver
            .execute_singleton_operation(op, OperationOptions::default())
            .await
            .map(|_| ())
    }

    // ---- diagnostics rendering ----

    /// Confirms which injected faults actually fired, scanning the per-request evaluations.
    fn fired_faults(ctx: &DiagnosticsContext) -> Vec<String> {
        let mut fired = Vec::new();
        for req in ctx.requests().iter() {
            for ev in req.fault_injection_evaluations() {
                if let FaultInjectionEvaluation::Applied { rule_id } = ev {
                    if !fired.contains(rule_id) {
                        fired.push(rule_id.clone());
                    }
                }
            }
        }
        fired
    }

    fn show_context(label: &str, ctx: &DiagnosticsContext) {
        println!("  -- live DiagnosticsContext: {label} --");
        super::summarize_attempts(ctx);
        let fired = fired_faults(ctx);
        if fired.is_empty() {
            println!("  injected faults fired: <none detected>");
        } else {
            println!("  injected faults fired (FaultInjectionEvaluation::Applied): {fired:?}");
        }
        let summary = ctx.summary();
        println!(
            "  summary: requests={} retries={} throttled={} RU={:.2} final={}",
            summary.request_count(),
            summary.retry_count(),
            summary.throttled_count(),
            summary.total_request_charge().value(),
            summary
                .final_status()
                .map(|s| s.to_string())
                .unwrap_or_else(|| "<none>".to_string()),
        );
        let json = ctx.encode(DiagnosticsEncoding::Json);
        let compact = ctx.encode(DiagnosticsEncoding::Compact);
        let encoded = ctx.encode(DiagnosticsEncoding::Encoded);
        println!(
            "  encoding sizes: Json={}B (pretty)  Compact={}B  Encoded={}B (base64)",
            json.len(),
            compact.len(),
            encoded.len()
        );
        println!("\n  canonical JSON (real timings):");
        super::print_pretty_json(ctx);
    }

    fn show_error_context(label: &str, err: &CosmosError) {
        println!(
            "  -- live operation FAILED: {label} (status {}) --",
            err.status()
        );
        match err.diagnostics() {
            Some(ctx) => show_context(label, &ctx),
            None => println!("  (no diagnostics attached to the error)"),
        }
    }
}
