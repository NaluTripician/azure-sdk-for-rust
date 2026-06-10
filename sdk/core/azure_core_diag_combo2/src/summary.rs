// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License.

//! The default **summary view** (D3 tier 1) and the detail→summary reducer.
//!
//! The summary is request-focused and aggregatable — close to today's request-style diagnostics —
//! so it is small, human-readable, and cheap to roll up across many operations. Its rules are
//! seeded from the TSG signal analysis (`DIAGNOSTICS-SIGNALS.md`): high-value signals (status
//! mix, throttling, total RU, the top error, retry count) are **always** surfaced; noisier
//! detail (per-attempt latency) is surfaced only when it crosses a threshold.

use crate::model::Captured;
use serde::Serialize;
use std::collections::BTreeMap;

/// Thresholds controlling which conditionally-surfaced signals appear in the summary.
#[derive(Clone, Copy, Debug)]
pub struct SummaryThresholds {
    /// Surface the slowest attempt's duration only when it exceeds this (ns).
    pub slow_attempt_ns: u64,
    /// Surface total request charge as a `high_charge` signal only when it exceeds this (RU).
    /// Mirrors Cosmos `DiagnosticsThresholds::request_charge_threshold`.
    pub request_charge_ru: f64,
}

impl Default for SummaryThresholds {
    fn default() -> Self {
        // 5 ms: high enough that fast point reads stay quiet, low enough to flag a slow hop.
        // 10 RU: a single cheap point read is ~1 RU; a heavy/fan-out query crosses this.
        Self {
            slow_attempt_ns: 5_000_000,
            request_charge_ru: 10.0,
        }
    }
}

/// The "top error" surfaced in a summary (the first non-2xx attempt).
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct TopError {
    /// HTTP status of the error.
    pub status: u16,
    /// Coarse error kind.
    pub error_kind: String,
    /// Service request id of the failing attempt, when available.
    pub service_request_id: Option<String>,
}

/// The default, aggregatable summary view.
#[derive(Clone, Debug, Serialize)]
pub struct Summary {
    /// Operation name.
    pub operation: String,
    /// `success` / `error`.
    pub outcome: &'static str,
    /// Whether the operation ultimately succeeded.
    pub succeeded: bool,
    /// Total attempt count.
    pub attempt_count: u32,
    /// Retries (attempts beyond the first).
    pub retry_count: u32,
    /// Total elapsed time (ns).
    pub total_elapsed_ns: u64,
    /// Summed request charge across attempts (RU).
    pub total_request_charge: f64,
    /// Count of attempts that were throttled (HTTP 429) — always surfaced.
    pub throttle_count: u32,
    /// Status-code histogram (status -> count) — always surfaced.
    pub status_counts: BTreeMap<String, u32>,
    /// Service request id of the final attempt (the one a customer quotes in a ticket).
    pub final_service_request_id: Option<String>,
    /// Number of fan-out children (the detail lives in the detailed view, not here).
    pub child_count: usize,
    /// The first error encountered, when the operation saw one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_error: Option<TopError>,
    /// The slowest attempt duration, surfaced only when it exceeds the threshold.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slow_attempt_ns: Option<u64>,
    /// Total request charge, surfaced only when it exceeds the RU threshold.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub high_charge: Option<f64>,
}

/// Reduces the detailed retained model into the summary view, applying `thresholds`.
pub fn reduce(captured: &Captured, thresholds: &SummaryThresholds) -> Summary {
    let mut status_counts: BTreeMap<String, u32> = BTreeMap::new();
    let mut total_request_charge = 0.0;
    let mut throttle_count = 0;
    let mut top_error: Option<TopError> = None;
    let mut slowest = 0u64;

    for attempt in &captured.attempts {
        *status_counts.entry(attempt.status.to_string()).or_insert(0) += 1;
        if let Some(ru) = attempt.request_charge {
            total_request_charge += ru;
        }
        if attempt.status == 429 {
            throttle_count += 1;
        }
        slowest = slowest.max(attempt.duration_ns);
        if top_error.is_none() {
            if let Some(kind) = attempt.error_kind() {
                top_error = Some(TopError {
                    status: attempt.status,
                    error_kind: kind.to_string(),
                    service_request_id: attempt.service_request_id.clone(),
                });
            }
        }
    }

    Summary {
        operation: captured.operation.clone(),
        outcome: if captured.succeeded {
            "success"
        } else {
            "error"
        },
        succeeded: captured.succeeded,
        attempt_count: captured.attempt_count,
        retry_count: captured.attempt_count.saturating_sub(1),
        total_elapsed_ns: captured.total_ns,
        total_request_charge,
        throttle_count,
        status_counts,
        final_service_request_id: captured
            .attempts
            .last()
            .and_then(|a| a.service_request_id.clone()),
        child_count: captured.children.len(),
        top_error,
        slow_attempt_ns: (slowest > thresholds.slow_attempt_ns).then_some(slowest),
        high_charge: (total_request_charge > thresholds.request_charge_ru)
            .then_some(total_request_charge),
    }
}
