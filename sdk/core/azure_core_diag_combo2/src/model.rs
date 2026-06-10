// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License.

//! The retained diagnostics model (C3) and its projection to the shared detailed [`WireTree`].
//!
//! This is a lightweight struct — not a full arena — because Combo 2's *default* output is the
//! summary, so the retained model only needs enough to (a) reduce to a summary cheaply and
//! (b) expand to the full detailed binary on demand or on error.

use azure_core_diag_common::attrs;
use azure_core_diag_common::wire::{NodeKind, WireNode, WireTree};

/// A recorded HTTP attempt.
#[derive(Clone, Debug)]
pub struct AttemptRec {
    /// Zero-based attempt index.
    pub index: u32,
    /// HTTP status code.
    pub status: u16,
    /// Service request id captured from the response.
    pub service_request_id: Option<String>,
    /// Request charge (RU).
    pub request_charge: Option<f64>,
    /// Start tick (ns).
    pub start_ns: u64,
    /// Duration (ns).
    pub duration_ns: u64,
}

impl AttemptRec {
    /// Whether this attempt succeeded (HTTP 2xx).
    pub fn succeeded(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// A coarse error-kind label for non-2xx attempts.
    pub fn error_kind(&self) -> Option<&'static str> {
        if self.succeeded() {
            return None;
        }
        Some(match self.status {
            429 => "throttled",
            404 => "not_found",
            400..=499 => "client_error",
            500..=599 => "server_error",
            _ => "unknown",
        })
    }
}

/// A recorded fan-out child (routing) span.
#[derive(Clone, Debug)]
pub struct ChildRec {
    /// Query-plan tree node id.
    pub plan_node_id: String,
    /// Feed range addressed by the child.
    pub feed_range: String,
    /// Start tick (ns).
    pub start_ns: u64,
    /// Duration (ns).
    pub duration_ns: u64,
}

/// The retained model populated during the request path.
#[derive(Clone, Debug, Default)]
pub struct Captured {
    /// Operation name.
    pub operation: String,
    /// Service endpoint.
    pub endpoint: String,
    /// Client request id.
    pub client_request_id: String,
    /// Recorded attempts.
    pub attempts: Vec<AttemptRec>,
    /// Recorded fan-out children.
    pub children: Vec<ChildRec>,
    /// Total attempt count.
    pub attempt_count: u32,
    /// Whether the operation ultimately succeeded.
    pub succeeded: bool,
    /// Operation start tick (ns).
    pub start_ns: u64,
    /// Total elapsed (ns).
    pub total_ns: u64,
}

impl Captured {
    /// Projects the retained model into the full detailed [`WireTree`] (the detailed-view construct step).
    pub fn to_wire(&self) -> WireTree {
        let mut nodes = Vec::with_capacity(1 + self.attempts.len() + self.children.len());
        nodes.push(WireNode {
            parent: None,
            kind: NodeKind::Operation as u8,
            start_ns: self.start_ns,
            duration_ns: self.total_ns,
            status: 0,
            attrs: vec![
                (attrs::ATTR_OPERATION.to_string(), self.operation.clone()),
                (attrs::ATTR_ENDPOINT.to_string(), self.endpoint.clone()),
                (
                    attrs::ATTR_CLIENT_REQUEST_ID.to_string(),
                    self.client_request_id.clone(),
                ),
                (
                    attrs::ATTR_ATTEMPT_COUNT.to_string(),
                    self.attempt_count.to_string(),
                ),
                (
                    "az.outcome".to_string(),
                    if self.succeeded { "success" } else { "error" }.to_string(),
                ),
            ],
        });
        for attempt in &self.attempts {
            let mut node_attrs = vec![
                ("attempt_index".to_string(), attempt.index.to_string()),
                (
                    attrs::ATTR_STATUS_CODE.to_string(),
                    attempt.status.to_string(),
                ),
            ];
            if let Some(svc) = &attempt.service_request_id {
                node_attrs.push((attrs::ATTR_SERVICE_REQUEST_ID.to_string(), svc.clone()));
            }
            if let Some(ru) = attempt.request_charge {
                node_attrs.push((attrs::ATTR_REQUEST_CHARGE.to_string(), ru.to_string()));
            }
            if let Some(kind) = attempt.error_kind() {
                node_attrs.push((attrs::ATTR_ERROR_KIND.to_string(), kind.to_string()));
            }
            nodes.push(WireNode {
                parent: Some(0),
                kind: NodeKind::Attempt as u8,
                start_ns: attempt.start_ns,
                duration_ns: attempt.duration_ns,
                status: attempt.status,
                attrs: node_attrs,
            });
        }
        for child in &self.children {
            nodes.push(WireNode {
                parent: Some(0),
                kind: NodeKind::Routing as u8,
                start_ns: child.start_ns,
                duration_ns: child.duration_ns,
                status: 0,
                attrs: vec![
                    (
                        attrs::ATTR_PLAN_NODE_ID.to_string(),
                        child.plan_node_id.clone(),
                    ),
                    (attrs::ATTR_FEED_RANGE.to_string(), child.feed_range.clone()),
                ],
            });
        }
        WireTree {
            operation: self.operation.clone(),
            nodes,
        }
    }
}
