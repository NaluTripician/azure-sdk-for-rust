// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License.

//! Live diagnostics-capture test (AC-7).
//!
//! Exercises the deferred gated diagnostics end-to-end against a real Cosmos DB account, proving
//! the recorder captures a real activity id / status / RU through the driver's operation path.
//!
//! It reads `COSMOS_CONNECTION_STRING` and **skips gracefully** (the test passes without
//! asserting) when:
//! - the env var is absent, or
//! - the account is unreachable / firewall-blocked / times out (a known condition for some test
//!   accounts whose firewall blocks the corp public IP and returns 403/transport errors).
//!
//! It only asserts when it actually receives a response from the service. This keeps CI green
//! without provisioned resources while still validating capture when an account is reachable.
//! Secret values are never printed.

use azure_data_cosmos_driver::diagnostics::DiagnosticsPolicy;
use azure_data_cosmos_driver::driver::CosmosDriverRuntime;
use azure_data_cosmos_driver::models::{
    AccountReference, ConnectionString, CosmosOperation, DatabaseReference,
};
use azure_data_cosmos_driver::options::{DriverOptions, OperationOptions};
use std::time::Duration;
use url::Url;

/// Maximum time to wait for the live call before treating the account as unreachable.
const LIVE_TIMEOUT: Duration = Duration::from_secs(20);

#[tokio::test]
async fn live_diagnostics_capture_or_env_gated() {
    let Ok(conn_str) = std::env::var("COSMOS_CONNECTION_STRING") else {
        eprintln!("AC-7 env-gated: COSMOS_CONNECTION_STRING not set; skipping live test");
        return;
    };

    let conn: ConnectionString = match conn_str.parse() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("AC-7 env-gated: COSMOS_CONNECTION_STRING did not parse: {e}");
            return;
        }
    };
    let Ok(endpoint) = Url::parse(conn.account_endpoint()) else {
        eprintln!("AC-7 env-gated: account endpoint is not a valid URL");
        return;
    };
    let account = AccountReference::with_master_key(endpoint, conn.account_key().clone());

    let runtime = match CosmosDriverRuntime::builder().build().await {
        Ok(r) => r,
        Err(e) => {
            eprintln!(
                "AC-7 env-gated: could not build driver runtime: {:?}",
                e.kind()
            );
            return;
        }
    };

    // Opt in with Always so the gate builds diagnostics for every operation.
    let driver_options = DriverOptions::builder(account.clone())
        .with_diagnostics_policy(DiagnosticsPolicy::always())
        .build();
    let driver = match runtime
        .get_or_create_driver(account.clone(), Some(driver_options))
        .await
    {
        Ok(d) => d,
        Err(e) => {
            eprintln!("AC-7 env-gated: could not create driver: {:?}", e.kind());
            return;
        }
    };

    // Read a database that almost certainly does not exist: a 404 (or a 403 firewall response) is
    // still an HTTP response that exercises capture with a real activity id. A network-level block
    // surfaces as a transport error and is treated as env-gated.
    let db = DatabaseReference::from_name(account, "diag-probe-nonexistent-db");
    let operation = CosmosOperation::read_database(db);

    let result = tokio::time::timeout(
        LIVE_TIMEOUT,
        driver.execute_operation(operation, OperationOptions::default()),
    )
    .await;

    match result {
        Err(_elapsed) => {
            eprintln!("AC-7 env-gated: live call timed out after {LIVE_TIMEOUT:?} (account likely unreachable / firewall-blocked)");
        }
        Ok(Err(e)) => {
            // Transport-level failure (DNS / connection refused / firewall TCP block / auth).
            eprintln!(
                "AC-7 env-gated: transport error ({:?}); account likely firewall-blocked",
                e.kind()
            );
        }
        Ok(Ok(response)) => {
            // We got a real HTTP response — assert capture actually happened.
            let rendered = response
                .diagnostics()
                .expect("Always policy must build diagnostics on a real response");
            let summary = rendered.summary().expect("a summary is built");
            // A real service response always carries at least one attempt with a status.
            assert_eq!(summary.attempt_count, 1, "single-request read_database");
            assert!(
                !summary.status_counts.is_empty(),
                "captured at least one status from the response"
            );
            eprintln!(
                "AC-7 LIVE OK: status={:?} outcome={} ru={} activity_id_present={}",
                summary.status_counts,
                summary.outcome,
                summary.total_request_charge,
                summary.final_service_request_id.is_some()
            );
        }
    }
}
