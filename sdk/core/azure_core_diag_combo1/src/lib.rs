// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License.

//! # Combo 1 — Span + Encoded (full-fidelity binary)
//!
//! **Stack:** A2/A3 span tree · C2 arena storage · D2 binary blob · D4 decode tool + skill.
//!
//! The headline idea: a full-fidelity span tree that is **cheap to build** (arena pushes),
//! **free to drop on success** (no serialization), and **small on the wire** (a compact binary
//! blob, not JSON). The arena→JSON pain is sidestepped by serializing the arena directly to the
//! shared `AZD1` binary format. A bundled `diag-decode` tool turns a blob back into JSON.
//!
//! See [`crate::arena`] for the storage and `SKILL.md` for the agent decode skill.

pub mod arena;

pub use arena::{Arena, Node};

use azure_core_diag_common::scenarios::{DiagSink, OperationInput, Outcome};
use azure_core_diag_common::wire::{decode, encode, encode_auto, DecodeError, NodeKind, WireTree};
use azure_core_diag_common::{attrs, MockClock};

/// The combo name used in samples and bench rows.
pub const COMBO: &str = "combo1";

fn error_kind(status: u16) -> &'static str {
    match status {
        429 => "throttled",
        404 => "not_found",
        400..=499 => "client_error",
        500..=599 => "server_error",
        _ => "unknown",
    }
}

/// A [`DiagSink`] that records into the arena (C2). Pushes only — no serialization happens here.
pub struct Combo1Sink {
    arena: Arena,
    root: u32,
}

impl Default for Combo1Sink {
    fn default() -> Self {
        Self {
            arena: Arena::new(),
            root: 0,
        }
    }
}

impl Combo1Sink {
    /// Consumes the sink and returns the populated [`Arena`].
    pub fn into_arena(self) -> Arena {
        self.arena
    }
}

impl DiagSink for Combo1Sink {
    fn op_start(&mut self, input: &OperationInput, start_ns: u64) {
        self.arena.operation = input.name.to_string();
        self.root = self.arena.push(None, NodeKind::Operation, start_ns, 0);
        self.arena
            .attr(self.root, attrs::ATTR_OPERATION, input.name);
        self.arena
            .attr(self.root, attrs::ATTR_ENDPOINT, input.endpoint);
        self.arena.attr(
            self.root,
            attrs::ATTR_CLIENT_REQUEST_ID,
            input.client_request_id,
        );
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
        let id = self
            .arena
            .push(Some(self.root), NodeKind::Attempt, start_ns, duration_ns);
        self.arena.set_status(id, status);
        self.arena
            .attr(id, "attempt_index", attempt_index.to_string());
        self.arena
            .attr(id, attrs::ATTR_STATUS_CODE, status.to_string());
        if let Some(svc) = service_request_id {
            self.arena.attr(id, attrs::ATTR_SERVICE_REQUEST_ID, svc);
        }
        if let Some(ru) = request_charge {
            self.arena
                .attr(id, attrs::ATTR_REQUEST_CHARGE, ru.to_string());
        }
        if !(200..300).contains(&status) {
            self.arena
                .attr(id, attrs::ATTR_ERROR_KIND, error_kind(status));
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
        let id = self
            .arena
            .push(Some(self.root), NodeKind::Routing, start_ns, duration_ns);
        self.arena.attr(id, attrs::ATTR_PLAN_NODE_ID, plan_node_id);
        self.arena.attr(id, attrs::ATTR_FEED_RANGE, feed_range);
    }

    fn op_end(&mut self, outcome: Outcome, attempt_count: u32, total_ns: u64) {
        self.arena.success = outcome == Outcome::Success;
        self.arena.set_duration(self.root, total_ns);
        self.arena.attr(
            self.root,
            attrs::ATTR_ATTEMPT_COUNT,
            attempt_count.to_string(),
        );
        self.arena.attr(
            self.root,
            "az.outcome",
            match outcome {
                Outcome::Success => "success",
                Outcome::Error => "error",
            },
        );
    }
}

/// Collects diagnostics for `input` into an arena (the hot-path cost: pushes only).
pub fn collect(input: &OperationInput, clock: &MockClock) -> Arena {
    let mut sink = Combo1Sink::default();
    azure_core_diag_common::drive_sink(input, &mut sink, clock);
    sink.into_arena()
}

/// Projects the arena into the shared [`WireTree`] (construct step).
pub fn construct(arena: &Arena) -> WireTree {
    arena.to_wire()
}

/// Encodes a [`WireTree`] to the binary blob, auto-compressing large trees.
pub fn serialize(tree: &WireTree) -> Vec<u8> {
    encode_auto(tree)
}

/// Encodes with explicit compression control (used by the harness to measure both).
pub fn serialize_with(tree: &WireTree, compress: bool) -> Vec<u8> {
    encode(tree, compress)
}

/// Decodes an `AZD1` blob back into a [`WireTree`] (off the hot path).
pub fn decode_blob(blob: &[u8]) -> Result<WireTree, DecodeError> {
    decode(blob)
}

/// The outcome-aware capture path: returns `None` (drop, no serialization) on success unless
/// `verbose` is set; otherwise returns the encoded binary blob.
pub fn capture_blob(input: &OperationInput, clock: &MockClock, verbose: bool) -> Option<Vec<u8>> {
    let arena = collect(input, clock);
    if arena.success && !verbose {
        // Free drop on the happy path — the arena is discarded without serializing.
        None
    } else {
        Some(serialize(&construct(&arena)))
    }
}

/// Async variant proving response-sourced capture into the arena.
pub async fn capture_blob_via_mock(
    input: &OperationInput,
    clock: &MockClock,
    verbose: bool,
) -> azure_core::Result<Option<Vec<u8>>> {
    let mut sink = Combo1Sink::default();
    azure_core_diag_common::drive_via_mock(input, &mut sink, clock).await?;
    let arena = sink.into_arena();
    Ok(if arena.success && !verbose {
        None
    } else {
        Some(serialize(&construct(&arena)))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use azure_core_diag_common::scenarios::{s1, s2, s3, s4};

    fn blob_for(input: &OperationInput) -> Vec<u8> {
        let clock = MockClock::new();
        let arena = collect(input, &clock);
        serialize(&construct(&arena))
    }

    #[test]
    fn s1_round_trips_through_decoder() {
        let blob = blob_for(&s1());
        let tree = decode_blob(&blob).unwrap();
        assert_eq!(tree.operation, "read_item");
        // node 0 = operation, node 1 = attempt
        let attempt = &tree.nodes[1];
        assert_eq!(attempt.status, 200);
        assert_eq!(
            attempt.attr(attrs::ATTR_SERVICE_REQUEST_ID),
            Some("svc-200")
        );
        assert_eq!(tree.nodes[0].attr("az.outcome"), Some("success"));
    }

    #[test]
    fn s2_round_trips_two_attempts() {
        let blob = blob_for(&s2());
        let tree = decode_blob(&blob).unwrap();
        let attempts: Vec<_> = tree
            .nodes
            .iter()
            .filter(|n| n.node_kind() == NodeKind::Attempt)
            .collect();
        assert_eq!(attempts.len(), 2);
        assert_eq!(attempts[0].status, 429);
        assert_eq!(
            attempts[0].attr(attrs::ATTR_SERVICE_REQUEST_ID),
            Some("svc-429")
        );
        assert_eq!(attempts[1].status, 200);
        assert_eq!(
            attempts[1].attr(attrs::ATTR_SERVICE_REQUEST_ID),
            Some("svc-200")
        );
    }

    #[test]
    fn s3_error_round_trips_and_drops_on_success_only() {
        let blob = blob_for(&s3());
        let tree = decode_blob(&blob).unwrap();
        let attempt = &tree.nodes[1];
        assert_eq!(attempt.status, 404);
        assert_eq!(
            attempt.attr(attrs::ATTR_SERVICE_REQUEST_ID),
            Some("svc-404")
        );
        assert_eq!(attempt.attr(attrs::ATTR_ERROR_KIND), Some("not_found"));

        // Outcome-aware: success drops (None), error serializes (Some).
        let clock = MockClock::new();
        assert!(capture_blob(&s1(), &clock, false).is_none());
        let clock = MockClock::new();
        assert!(capture_blob(&s3(), &clock, false).is_some());
    }

    #[test]
    fn blob_is_smaller_than_combo3_json() {
        for input in [s1(), s2(), s3(), s4(10), s4(25)] {
            let clock = MockClock::new();
            let combo3_json = azure_core_diag_combo3::to_json_compact(
                &azure_core_diag_combo3::capture(&input, &clock),
            );
            let blob = blob_for(&input);
            assert!(
                blob.len() < combo3_json.len(),
                "scenario {}: blob {} not < json {}",
                input.name,
                blob.len(),
                combo3_json.len()
            );
        }
    }

    #[test]
    fn s4_verbose_round_trips_and_stays_small() {
        let input = s4(25);
        let blob = blob_for(&input);
        let tree = decode_blob(&blob).unwrap();
        let children: Vec<_> = tree
            .nodes
            .iter()
            .filter(|n| n.node_kind() == NodeKind::Routing)
            .collect();
        assert_eq!(children.len(), 25);
        assert_eq!(
            children[0].attr(attrs::ATTR_PLAN_NODE_ID),
            Some("plan/node/0")
        );

        let clock = MockClock::new();
        let combo3_json = azure_core_diag_combo3::to_json_compact(
            &azure_core_diag_combo3::capture(&input, &clock),
        );
        assert!(blob.len() < combo3_json.len());
    }

    #[tokio::test]
    async fn via_mock_sources_id_from_response() {
        let clock = MockClock::new();
        let blob = capture_blob_via_mock(&s3(), &clock, false)
            .await
            .unwrap()
            .expect("error path serializes");
        let tree = decode_blob(&blob).unwrap();
        assert_eq!(
            tree.nodes[1].attr(attrs::ATTR_SERVICE_REQUEST_ID),
            Some("svc-404")
        );
    }
}
