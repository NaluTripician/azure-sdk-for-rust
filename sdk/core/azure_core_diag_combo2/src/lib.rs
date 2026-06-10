// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License.

//! # Combo 2 — Tiered hybrid (migration-friendly)
//!
//! **Stack:** A3 hybrid · B3 wrapper types · C3 outcome-aware · D3 tiered verbosity
//! (default summarized JSON, detailed = binary).
//!
//! The customer-friendly path. The **default** output is a small, human-readable, aggregatable
//! *summary* (≈ today's request-style diagnostics). Full detail — the complete span tree as a
//! binary blob — is produced only on demand or automatically on error.
//!
//! The capture mechanism is **wrapper types (B3)**: a single emit call-site both emits an internal
//! `tracing` event (for anyone already consuming `tracing`) *and* records into the retained model.

mod model;
mod summary;

pub use model::{AttemptRec, Captured, ChildRec};
pub use summary::{reduce, Summary, SummaryThresholds, TopError};

use azure_core_diag_common::scenarios::{DiagSink, OperationInput, Outcome};
use azure_core_diag_common::wire::{encode_auto, WireTree};
use azure_core_diag_common::MockClock;

/// The combo name used in samples and bench rows.
pub const COMBO: &str = "combo2";

/// Requested diagnostics verbosity (the granularity knob).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verbosity {
    /// Default: emit only the aggregatable summary.
    Summary,
    /// Emit the full detailed span tree (binary).
    Detailed,
}

/// Applies the escalate-on-error rule: a `Summary` request is auto-upgraded to `Detailed` when
/// the operation failed, so error diagnostics are never lossy.
pub fn effective_verbosity(requested: Verbosity, succeeded: bool) -> Verbosity {
    match requested {
        Verbosity::Detailed => Verbosity::Detailed,
        Verbosity::Summary => {
            if succeeded {
                Verbosity::Summary
            } else {
                Verbosity::Detailed
            }
        }
    }
}

/// The rendered diagnostics for one operation.
#[derive(Clone, Debug)]
pub enum Rendered {
    /// Default tier: compact summary JSON.
    Summary {
        /// Compact summary JSON bytes.
        json: Vec<u8>,
    },
    /// Detailed tier: the summary plus the full detailed binary blob.
    Detailed {
        /// Compact summary JSON bytes (kept so the summary is always available).
        json: Vec<u8>,
        /// The full span tree as an `AZD1` binary blob.
        blob: Vec<u8>,
    },
}

impl Rendered {
    /// The summary JSON, present in both tiers.
    pub fn summary_json(&self) -> &[u8] {
        match self {
            Rendered::Summary { json } | Rendered::Detailed { json, .. } => json,
        }
    }

    /// The detailed binary blob, present only in the detailed tier.
    pub fn detailed_blob(&self) -> Option<&[u8]> {
        match self {
            Rendered::Detailed { blob, .. } => Some(blob),
            Rendered::Summary { .. } => None,
        }
    }
}

/// The B3 wrapper sink: every emit call-site dual-writes to `tracing` and the retained model.
#[derive(Default)]
pub struct Combo2Sink {
    captured: Captured,
}

impl Combo2Sink {
    /// Consumes the sink and returns the retained model.
    pub fn into_captured(self) -> Captured {
        self.captured
    }
}

impl DiagSink for Combo2Sink {
    fn op_start(&mut self, input: &OperationInput, start_ns: u64) {
        // Wrapper: feed the live tracing pipeline...
        tracing::debug!(target: "az.diagnostics", operation = input.name, "operation start");
        // ...and the retained model.
        self.captured.operation = input.name.to_string();
        self.captured.endpoint = input.endpoint.to_string();
        self.captured.client_request_id = input.client_request_id.to_string();
        self.captured.start_ns = start_ns;
    }

    fn attempt(
        &mut self,
        attempt_index: u32,
        status: u16,
        service_request_id: Option<&str>,
        request_charge: Option<f64>,
        start_ns: u64,
        duration_ns: u64,
    ) {
        tracing::debug!(
            target: "az.diagnostics",
            attempt_index,
            status = status as u64,
            "attempt"
        );
        self.captured.attempts.push(AttemptRec {
            index: attempt_index,
            status,
            service_request_id: service_request_id.map(str::to_string),
            request_charge,
            start_ns,
            duration_ns,
        });
    }

    fn child(
        &mut self,
        _child_index: u32,
        plan_node_id: &str,
        feed_range: &str,
        start_ns: u64,
        duration_ns: u64,
    ) {
        tracing::trace!(target: "az.diagnostics", plan_node_id, "routing child");
        self.captured.children.push(ChildRec {
            plan_node_id: plan_node_id.to_string(),
            feed_range: feed_range.to_string(),
            start_ns,
            duration_ns,
        });
    }

    fn op_end(&mut self, outcome: Outcome, attempt_count: u32, total_ns: u64) {
        self.captured.succeeded = outcome == Outcome::Success;
        self.captured.attempt_count = attempt_count;
        self.captured.total_ns = total_ns;
        tracing::debug!(
            target: "az.diagnostics",
            attempt_count,
            "operation end"
        );
    }
}

/// Collects diagnostics into the retained model (hot path: record + emit tracing events).
pub fn collect(input: &OperationInput, clock: &MockClock) -> Captured {
    let mut sink = Combo2Sink::default();
    azure_core_diag_common::drive_sink(input, &mut sink, clock);
    sink.into_captured()
}

/// Builds the summary view (construct, summary tier).
pub fn summarize(captured: &Captured, thresholds: &SummaryThresholds) -> Summary {
    reduce(captured, thresholds)
}

/// Serializes a [`Summary`] to compact JSON (the measured default output).
pub fn to_summary_json(summary: &Summary) -> Vec<u8> {
    serde_json::to_vec(summary).expect("Summary serializes")
}

/// Serializes a [`Summary`] to pretty JSON (for the sample gallery).
pub fn to_summary_json_pretty(summary: &Summary) -> String {
    serde_json::to_string_pretty(summary).expect("Summary serializes")
}

/// Builds the detailed span tree (construct, detailed tier).
pub fn detailed_wire(captured: &Captured) -> WireTree {
    captured.to_wire()
}

/// Serializes the detailed span tree to the binary blob.
pub fn detailed_blob(captured: &Captured) -> Vec<u8> {
    encode_auto(&captured.to_wire())
}

/// End-to-end render honoring the verbosity knob and the escalate-on-error rule.
pub fn render(
    input: &OperationInput,
    clock: &MockClock,
    requested: Verbosity,
    thresholds: &SummaryThresholds,
) -> Rendered {
    let captured = collect(input, clock);
    let json = to_summary_json(&summarize(&captured, thresholds));
    match effective_verbosity(requested, captured.succeeded) {
        Verbosity::Summary => Rendered::Summary { json },
        Verbosity::Detailed => Rendered::Detailed {
            json,
            blob: detailed_blob(&captured),
        },
    }
}

/// Async variant proving response-sourced capture, honoring the verbosity knob.
pub async fn render_via_mock(
    input: &OperationInput,
    clock: &MockClock,
    requested: Verbosity,
    thresholds: &SummaryThresholds,
) -> azure_core::Result<Rendered> {
    let mut sink = Combo2Sink::default();
    azure_core_diag_common::drive_via_mock(input, &mut sink, clock).await?;
    let captured = sink.into_captured();
    let json = to_summary_json(&summarize(&captured, thresholds));
    Ok(match effective_verbosity(requested, captured.succeeded) {
        Verbosity::Summary => Rendered::Summary { json },
        Verbosity::Detailed => Rendered::Detailed {
            json,
            blob: detailed_blob(&captured),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use azure_core_diag_common::scenarios::{s1, s2, s3, s4};
    use azure_core_diag_common::wire::{decode, NodeKind};

    fn summary_of(input: &OperationInput) -> Summary {
        let clock = MockClock::new();
        let captured = collect(input, &clock);
        summarize(&captured, &SummaryThresholds::default())
    }

    #[test]
    fn s1_default_is_summary_with_aggregates() {
        let clock = MockClock::new();
        let rendered = render(
            &s1(),
            &clock,
            Verbosity::Summary,
            &SummaryThresholds::default(),
        );
        assert!(matches!(rendered, Rendered::Summary { .. }));
        let summary = summary_of(&s1());
        assert!(summary.succeeded);
        assert_eq!(summary.attempt_count, 1);
        assert_eq!(summary.total_request_charge, 4.2);
        assert_eq!(summary.status_counts.get("200"), Some(&1));
        // Fast point read does not surface a slow-attempt signal.
        assert_eq!(summary.slow_attempt_ns, None);
        // 4.2 RU is below the 10 RU threshold, so no high-charge signal.
        assert_eq!(summary.high_charge, None);
    }

    #[test]
    fn s2_summary_aggregates_throttle_and_retry() {
        let summary = summary_of(&s2());
        assert_eq!(summary.attempt_count, 2);
        assert_eq!(summary.retry_count, 1);
        assert_eq!(summary.throttle_count, 1);
        assert_eq!(summary.status_counts.get("429"), Some(&1));
        assert_eq!(summary.status_counts.get("200"), Some(&1));
        assert!((summary.total_request_charge - 8.4).abs() < 1e-9);
        assert_eq!(summary.final_service_request_id.as_deref(), Some("svc-200"));
    }

    #[test]
    fn s3_auto_escalates_to_detailed_binary() {
        let clock = MockClock::new();
        let rendered = render(
            &s3(),
            &clock,
            Verbosity::Summary,
            &SummaryThresholds::default(),
        );
        // Escalate-on-error: a Summary request becomes Detailed when the op failed.
        let blob = rendered
            .detailed_blob()
            .expect("error escalates to detailed");
        let tree = decode(blob).unwrap();
        assert_eq!(
            tree.nodes[1].attr(azure_core_diag_common::attrs::ATTR_SERVICE_REQUEST_ID),
            Some("svc-404")
        );
        assert_eq!(
            tree.nodes[1].attr(azure_core_diag_common::attrs::ATTR_ERROR_KIND),
            Some("not_found")
        );

        // The summary is still present alongside the detailed blob.
        let summary: serde_json::Value = serde_json::from_slice(rendered.summary_json()).unwrap();
        assert_eq!(summary["outcome"], "error");
        assert_eq!(summary["top_error"]["status"], 404);
    }

    #[test]
    fn s4_summary_stays_small_detailed_has_all_children() {
        let clock = MockClock::new();
        let captured = collect(&s4(25), &clock);
        let summary_json = to_summary_json(&summarize(&captured, &SummaryThresholds::default()));
        let blob = detailed_blob(&captured);

        // Summary omits per-child detail (only a count), so it is far smaller than the detailed blob.
        assert!(
            summary_json.len() < blob.len(),
            "summary {} should be < detailed {}",
            summary_json.len(),
            blob.len()
        );

        let tree = decode(&blob).unwrap();
        let children = tree
            .nodes
            .iter()
            .filter(|n| n.node_kind() == NodeKind::Routing)
            .count();
        assert_eq!(children, 25);

        // S4's 6ms attempt exceeds the 5ms slow threshold and is surfaced.
        let summary = summarize(&captured, &SummaryThresholds::default());
        assert_eq!(summary.child_count, 25);
        assert_eq!(summary.slow_attempt_ns, Some(6_000_000));
        // S4's 18.6 RU query crosses the 10 RU threshold.
        assert_eq!(summary.high_charge, Some(18.6));
    }

    #[test]
    fn detailed_requested_includes_blob_even_on_success() {
        let clock = MockClock::new();
        let rendered = render(
            &s1(),
            &clock,
            Verbosity::Detailed,
            &SummaryThresholds::default(),
        );
        assert!(rendered.detailed_blob().is_some());
    }

    #[tokio::test]
    async fn via_mock_sources_id_from_response() {
        let clock = MockClock::new();
        let rendered = render_via_mock(
            &s3(),
            &clock,
            Verbosity::Summary,
            &SummaryThresholds::default(),
        )
        .await
        .unwrap();
        let summary: serde_json::Value = serde_json::from_slice(rendered.summary_json()).unwrap();
        assert_eq!(summary["final_service_request_id"], "svc-404");
    }
}
