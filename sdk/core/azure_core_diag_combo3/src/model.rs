// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License.

//! The C1 eager structured objects and their projection from the captured [`Store`].
//!
//! "Eager" means the full typed object graph is materialized up front (even on success) and
//! then serialized as human-readable JSON (D1). There is no outcome-aware drop trick — this is
//! the baseline cost every other combo is compared against.

use crate::collector::Store;
use azure_core_diag_common::attrs;
use serde::Serialize;
use serde_json::{Map, Value};

/// A captured event (e.g. the response/error record on an attempt).
#[derive(Clone, Debug, Default, Serialize)]
pub struct EventRecord {
    /// The event fields as captured from `tracing`.
    #[serde(flatten)]
    pub fields: Map<String, Value>,
}

/// A single HTTP attempt span.
#[derive(Clone, Debug, Default, Serialize)]
pub struct AttemptSpan {
    /// Zero-based attempt index.
    pub attempt_index: u64,
    /// HTTP status code.
    pub status: u64,
    /// Service request id captured from the response.
    pub service_request_id: Option<String>,
    /// Request charge (RU).
    pub request_charge: Option<f64>,
    /// Start tick (ns).
    pub start_ns: u64,
    /// Duration (ns).
    pub duration_ns: u64,
    /// Events emitted within the attempt.
    pub events: Vec<EventRecord>,
}

/// A fan-out child (routing) span.
#[derive(Clone, Debug, Default, Serialize)]
pub struct ChildSpan {
    /// Query-plan tree node id.
    pub plan_node_id: Option<String>,
    /// Feed range addressed by the child.
    pub feed_range: Option<String>,
    /// Start tick (ns).
    pub start_ns: u64,
    /// Duration (ns).
    pub duration_ns: u64,
}

/// The root operation span: a fully-materialized, eager object graph.
#[derive(Clone, Debug, Default, Serialize)]
pub struct OperationSpan {
    /// Operation name.
    pub operation: Option<String>,
    /// Service endpoint.
    pub endpoint: Option<String>,
    /// Client request id.
    pub client_request_id: Option<String>,
    /// Final outcome (`success`/`error`).
    pub outcome: Option<String>,
    /// Total attempt count.
    pub attempt_count: u64,
    /// Start tick (ns).
    pub start_ns: u64,
    /// Total duration (ns).
    pub duration_ns: u64,
    /// Per-attempt spans.
    pub attempts: Vec<AttemptSpan>,
    /// Fan-out child spans.
    pub children: Vec<ChildSpan>,
}

fn field_str(fields: &std::collections::BTreeMap<String, Value>, key: &str) -> Option<String> {
    fields.get(key).map(|v| match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    })
}

fn field_u64(fields: &std::collections::BTreeMap<String, Value>, key: &str) -> u64 {
    fields.get(key).and_then(Value::as_u64).unwrap_or_default()
}

fn field_f64(fields: &std::collections::BTreeMap<String, Value>, key: &str) -> Option<f64> {
    fields.get(key).and_then(Value::as_f64)
}

/// Projects the raw capture [`Store`] into the eager [`OperationSpan`] object graph (C1 + construct).
pub fn project(store: &Store) -> OperationSpan {
    let Some(root_id) = store.root() else {
        return OperationSpan::default();
    };
    let root = &store.spans[&root_id];

    let mut op = OperationSpan {
        operation: field_str(&root.fields, attrs::ATTR_OPERATION),
        endpoint: field_str(&root.fields, attrs::ATTR_ENDPOINT),
        client_request_id: field_str(&root.fields, attrs::ATTR_CLIENT_REQUEST_ID),
        outcome: field_str(&root.fields, "az.outcome"),
        attempt_count: field_u64(&root.fields, attrs::ATTR_ATTEMPT_COUNT),
        start_ns: field_u64(&root.fields, "az.start_ns"),
        duration_ns: field_u64(&root.fields, "az.duration_ns"),
        attempts: Vec::new(),
        children: Vec::new(),
    };

    for child_id in store.children(root_id) {
        let span = &store.spans[&child_id];
        match span.name.as_str() {
            "attempt" => {
                op.attempts.push(AttemptSpan {
                    attempt_index: field_u64(&span.fields, "attempt_index"),
                    status: field_u64(&span.fields, attrs::ATTR_STATUS_CODE),
                    service_request_id: field_str(&span.fields, attrs::ATTR_SERVICE_REQUEST_ID),
                    request_charge: field_f64(&span.fields, attrs::ATTR_REQUEST_CHARGE),
                    start_ns: field_u64(&span.fields, "az.start_ns"),
                    duration_ns: field_u64(&span.fields, "az.duration_ns"),
                    events: span
                        .events
                        .iter()
                        .map(|fields| EventRecord {
                            fields: fields.clone(),
                        })
                        .collect(),
                });
            }
            "routing" => {
                op.children.push(ChildSpan {
                    plan_node_id: field_str(&span.fields, attrs::ATTR_PLAN_NODE_ID),
                    feed_range: field_str(&span.fields, attrs::ATTR_FEED_RANGE),
                    start_ns: field_u64(&span.fields, "az.start_ns"),
                    duration_ns: field_u64(&span.fields, "az.duration_ns"),
                });
            }
            _ => {}
        }
    }

    op
}
