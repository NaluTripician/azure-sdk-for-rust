// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License.

//! # Combo 3 — tracing-native baseline / strawman
//!
//! **Stack:** A2 span hierarchy · B2 `tracing`-crate-native · C1 eager structured objects · D1 JSON.
//!
//! This is the cheapest-to-build, most idiomatic design and the **baseline** every other combo
//! is benchmarked against. An outer `operation` span wraps the retries, each attempt is an
//! `attempt` span, and fan-out produces `routing` child spans. A capturing
//! [`SpanCollector`](collector::SpanCollector) layer eagerly materializes the whole tree into
//! typed objects ([`OperationSpan`]) which are serialized to human-readable JSON.
//!
//! Because the subscriber processes everything eagerly, the full cost is paid **even on the
//! happy path** — there is no outcome-aware drop trick. That is the headline weakness this
//! baseline exists to expose.

mod collector;
mod model;

pub use collector::{SharedStore, SpanCollector, Store};
pub use model::{AttemptSpan, ChildSpan, EventRecord, OperationSpan};

use azure_core_diag_common::scenarios::{DiagSink, OperationInput, Outcome};
use azure_core_diag_common::MockClock;
use std::sync::{Arc, Mutex};
use tracing::field::Empty;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::registry::Registry;

/// The combo name used in samples and bench rows.
pub const COMBO: &str = "combo3";

fn error_kind(status: u16) -> &'static str {
    match status {
        429 => "throttled",
        404 => "not_found",
        400..=499 => "client_error",
        500..=599 => "server_error",
        _ => "unknown",
    }
}

/// A [`DiagSink`] that emits `tracing` spans and events. Combined with [`SpanCollector`], this
/// is the B2 (`tracing`-native) capture mechanism.
#[derive(Default)]
pub struct Combo3Sink {
    op_span: Option<tracing::Span>,
}

impl DiagSink for Combo3Sink {
    fn op_start(&mut self, input: &OperationInput, start_ns: u64) {
        let span = tracing::info_span!(
            "operation",
            az.operation = input.name,
            az.endpoint = input.endpoint,
            az.client_request_id = input.client_request_id,
            az.start_ns = start_ns,
            az.attempt_count = Empty,
            az.duration_ns = Empty,
            az.outcome = Empty,
        );
        self.op_span = Some(span);
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
        let Some(op) = self.op_span.as_ref() else {
            return;
        };
        let span = tracing::info_span!(
            parent: op,
            "attempt",
            attempt_index = attempt_index as u64,
            az.status_code = status as u64,
            az.service_request_id = service_request_id.unwrap_or_default(),
            az.request_charge = request_charge.unwrap_or_default(),
            az.start_ns = start_ns,
            az.duration_ns = duration_ns,
        );
        if (200..300).contains(&status) {
            tracing::info!(
                parent: &span,
                az.event = "response",
                az.status_code = status as u64,
            );
        } else {
            tracing::warn!(
                parent: &span,
                az.event = "error",
                az.status_code = status as u64,
                az.error_kind = error_kind(status),
            );
        }
    }

    fn child(
        &mut self,
        _child_index: u32,
        plan_node_id: &str,
        feed_range: &str,
        start_ns: u64,
        duration_ns: u64,
    ) {
        let Some(op) = self.op_span.as_ref() else {
            return;
        };
        let _span = tracing::info_span!(
            parent: op,
            "routing",
            az.plan_node_id = plan_node_id,
            az.feed_range = feed_range,
            az.start_ns = start_ns,
            az.duration_ns = duration_ns,
        );
    }

    fn op_end(&mut self, outcome: Outcome, attempt_count: u32, total_ns: u64) {
        if let Some(op) = self.op_span.as_ref() {
            op.record("az.attempt_count", attempt_count as u64);
            op.record("az.duration_ns", total_ns);
            op.record(
                "az.outcome",
                match outcome {
                    Outcome::Success => "success",
                    Outcome::Error => "error",
                },
            );
        }
        // Dropping the span closes it; nothing is discarded — the layer already captured it.
        self.op_span = None;
    }
}

fn take_store(store: SharedStore) -> Store {
    Arc::try_unwrap(store)
        .map(|m| m.into_inner().unwrap())
        .unwrap_or_else(|arc| arc.lock().unwrap().clone())
}

/// Collects diagnostics for `input` by driving a [`Combo3Sink`] under a capturing subscriber.
///
/// This is the hot-path cost: span creation + layer capture. Returns the raw [`Store`].
pub fn collect(input: &OperationInput, clock: &MockClock) -> Store {
    let store: SharedStore = Arc::new(Mutex::new(Store::default()));
    let subscriber = Registry::default().with(SpanCollector::new(store.clone()));
    tracing::subscriber::with_default(subscriber, || {
        let mut sink = Combo3Sink::default();
        azure_core_diag_common::drive_sink(input, &mut sink, clock);
    });
    take_store(store)
}

/// Materializes the eager [`OperationSpan`] object graph from a captured store (the C1 construct step).
pub fn construct(store: &Store) -> OperationSpan {
    model::project(store)
}

/// Serializes an [`OperationSpan`] to compact JSON (the form measured for output size).
pub fn to_json_compact(op: &OperationSpan) -> Vec<u8> {
    serde_json::to_vec(op).expect("OperationSpan serializes")
}

/// Serializes an [`OperationSpan`] to pretty JSON (for the sample gallery).
pub fn to_json_pretty(op: &OperationSpan) -> String {
    serde_json::to_string_pretty(op).expect("OperationSpan serializes")
}

/// Convenience: collect + construct in one call.
pub fn capture(input: &OperationInput, clock: &MockClock) -> OperationSpan {
    let store = collect(input, clock);
    construct(&store)
}

/// Async variant proving response-sourced capture: routes attempts through a mock HTTP client
/// and reads the service request id back out of the response headers.
pub async fn capture_via_mock(
    input: &OperationInput,
    clock: &MockClock,
) -> azure_core::Result<OperationSpan> {
    let store: SharedStore = Arc::new(Mutex::new(Store::default()));
    let subscriber = Registry::default().with(SpanCollector::new(store.clone()));
    let guard = tracing::subscriber::set_default(subscriber);
    let mut sink = Combo3Sink::default();
    azure_core_diag_common::drive_via_mock(input, &mut sink, clock).await?;
    drop(guard);
    let store = store.lock().unwrap().clone();
    Ok(construct(&store))
}

#[cfg(test)]
mod tests {
    use super::*;
    use azure_core_diag_common::scenarios::{s1, s2, s3, s4};

    #[test]
    fn s1_single_success() {
        let clock = MockClock::new();
        let op = capture(&s1(), &clock);
        assert_eq!(op.operation.as_deref(), Some("read_item"));
        assert_eq!(op.outcome.as_deref(), Some("success"));
        assert_eq!(op.attempt_count, 1);
        assert_eq!(op.attempts.len(), 1);
        assert_eq!(op.attempts[0].status, 200);
        assert_eq!(
            op.attempts[0].service_request_id.as_deref(),
            Some("svc-200")
        );
        assert_eq!(op.attempts[0].request_charge, Some(4.2));
    }

    #[test]
    fn s2_retry_then_success() {
        let clock = MockClock::new();
        let op = capture(&s2(), &clock);
        assert_eq!(op.attempt_count, 2);
        assert_eq!(op.outcome.as_deref(), Some("success"));
        assert_eq!(op.attempts[0].status, 429);
        assert_eq!(
            op.attempts[0].service_request_id.as_deref(),
            Some("svc-429")
        );
        assert_eq!(op.attempts[1].status, 200);
        assert_eq!(
            op.attempts[1].service_request_id.as_deref(),
            Some("svc-200")
        );
    }

    #[test]
    fn s3_error_path_captures_service_id() {
        let clock = MockClock::new();
        let op = capture(&s3(), &clock);
        assert_eq!(op.outcome.as_deref(), Some("error"));
        assert_eq!(op.attempts.len(), 1);
        assert_eq!(op.attempts[0].status, 404);
        assert_eq!(
            op.attempts[0].service_request_id.as_deref(),
            Some("svc-404")
        );
    }

    #[test]
    fn s4_fanout_produces_children() {
        for n in [10usize, 25] {
            let clock = MockClock::new();
            let op = capture(&s4(n), &clock);
            assert_eq!(op.children.len(), n);
            assert_eq!(op.attempts[0].status, 200);
            assert_eq!(op.children[0].plan_node_id.as_deref(), Some("plan/node/0"));
            assert!(op.children[0].feed_range.is_some());
        }
    }

    #[test]
    fn serializes_to_json() {
        let clock = MockClock::new();
        let op = capture(&s2(), &clock);
        let json = to_json_compact(&op);
        let parsed: serde_json::Value = serde_json::from_slice(&json).unwrap();
        assert_eq!(parsed["attempt_count"], 2);
        assert_eq!(parsed["attempts"][0]["service_request_id"], "svc-429");
    }

    #[tokio::test]
    async fn capture_via_mock_sources_id_from_response() {
        let clock = MockClock::new();
        let op = capture_via_mock(&s3(), &clock).await.unwrap();
        assert_eq!(
            op.attempts[0].service_request_id.as_deref(),
            Some("svc-404")
        );
        assert_eq!(op.outcome.as_deref(), Some("error"));
    }
}
