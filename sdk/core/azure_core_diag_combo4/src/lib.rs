// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License.

//! # Combo 4 — Deferred, threshold-gated capture
//!
//! **Stack:** A3 hybrid · append-only pooled capture · C3 outcome+latency gate · D2/D3 build-on-demand.
//!
//! The design distilled from the team discussion. It pushes the [`Binary Span Tree`](../azure_core_diag_combo1)
//! and [`Tiered Hybrid`](../azure_core_diag_combo2) ideas to their conclusion:
//!
//! 1. **Hot path — write-preferred, compact, append-only.** Each operation rents a `Vec<u8>` from a
//!    [`LogPool`] and appends a raw TLV record stream ([`CaptureLog`]). Attribute keys are implicit
//!    in the fixed record layout (no key bytes), numbers are varints, and the version/User-Agent
//!    provenance is a single byte referencing a process-global [`preamble`]. Nothing is formatted
//!    and almost nothing is allocated.
//! 2. **Gate — decide at the end.** When the outcome and elapsed time are known, a
//!    [`DiagnosticsPolicy`] decides whether the diagnostics are worth building (slow? errored?
//!    always-on?). If not, the buffer is returned to the pool — **~free**.
//! 3. **Build — only when wanted.** If the gate says yes, the raw log is parsed and rendered to a
//!    compact summary (and, opt-in, the `AZD1` binary blob — reusing the shared codec/tool). We pay
//!    the string-building cost only in the case where we already know we want the output.

mod preamble;

pub use preamble::Preamble;

use azure_core_diag_common::attrs;
use azure_core_diag_common::scenarios::{DiagSink, OperationInput, Outcome};
use azure_core_diag_common::wire::{self, NodeKind, WireNode, WireTree};
use azure_core_diag_common::MockClock;
use serde::Serialize;
use std::collections::BTreeMap;

/// The combo name used in samples and bench rows.
pub const COMBO: &str = "combo4";

#[repr(u8)]
enum Tag {
    Op = 1,
    Attempt = 2,
    Child = 3,
    End = 4,
}

// ---------------------------------------------------------------------------
// Pool + capture log (the hot path)
// ---------------------------------------------------------------------------

/// A pool of reusable capture buffers. Renting and returning keeps the happy-path "drop" cost to a
/// `clear()` plus a `push` — no allocation, no free.
#[derive(Debug, Default)]
pub struct LogPool {
    free: Vec<Vec<u8>>,
}

impl LogPool {
    /// Creates an empty pool.
    pub fn new() -> Self {
        Self::default()
    }

    /// Rents a cleared buffer, reusing a pooled one when available.
    fn rent(&mut self) -> Vec<u8> {
        match self.free.pop() {
            Some(mut buf) => {
                buf.clear();
                buf
            }
            None => Vec::with_capacity(256),
        }
    }

    /// Returns a buffer to the pool (cleared, capacity retained).
    fn give_back(&mut self, mut buf: Vec<u8>) {
        buf.clear();
        self.free.push(buf);
    }

    /// Number of buffers currently parked in the pool.
    pub fn pooled(&self) -> usize {
        self.free.len()
    }
}

/// A per-operation append-only capture log. All writes are appends to a single pooled buffer.
#[derive(Debug)]
pub struct CaptureLog {
    buf: Vec<u8>,
    start_ns: u64,
    total_ns: u64,
    outcome: Outcome,
    attempt_count: u32,
}

impl CaptureLog {
    fn from_buf(buf: Vec<u8>) -> Self {
        Self {
            buf,
            start_ns: 0,
            total_ns: 0,
            outcome: Outcome::Success,
            attempt_count: 0,
        }
    }

    /// Consumes the log and returns the backing buffer (for returning to a pool).
    pub fn into_buf(self) -> Vec<u8> {
        self.buf
    }

    /// The recorded operation outcome.
    pub fn outcome(&self) -> Outcome {
        self.outcome
    }

    /// The recorded total elapsed time (nanoseconds, mock clock).
    pub fn elapsed_ns(&self) -> u64 {
        self.total_ns
    }

    /// The raw, compact on-the-wire size of what was appended on the hot path.
    pub fn raw_len(&self) -> usize {
        self.buf.len()
    }
}

/// A [`DiagSink`] that appends to a [`CaptureLog`]. This is the entire hot-path cost.
pub struct Combo4Sink {
    log: CaptureLog,
}

impl Combo4Sink {
    fn new(buf: Vec<u8>) -> Self {
        Self {
            log: CaptureLog::from_buf(buf),
        }
    }

    fn into_log(self) -> CaptureLog {
        self.log
    }
}

impl DiagSink for Combo4Sink {
    fn op_start(&mut self, input: &OperationInput, start_ns: u64) {
        self.log.start_ns = start_ns;
        let b = &mut self.log.buf;
        b.push(Tag::Op as u8);
        b.push(preamble::PREAMBLE_ID); // 1 byte = full version/UA provenance
        wire::write_str(b, input.name);
        wire::write_str(b, input.endpoint);
        wire::write_str(b, input.client_request_id);
        wire::write_varint(b, start_ns);
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
        let b = &mut self.log.buf;
        b.push(Tag::Attempt as u8);
        wire::write_varint(b, u64::from(attempt_index));
        wire::write_varint(b, u64::from(status));
        wire::write_str(b, service_request_id.unwrap_or_default());
        b.extend_from_slice(&(request_charge.unwrap_or(0.0) as f32).to_le_bytes());
        wire::write_varint(b, start_ns);
        wire::write_varint(b, duration_ns);
    }

    fn child(
        &mut self,
        _child_index: u32,
        plan_node_id: &str,
        feed_range: &str,
        start_ns: u64,
        duration_ns: u64,
    ) {
        let b = &mut self.log.buf;
        b.push(Tag::Child as u8);
        wire::write_str(b, plan_node_id);
        wire::write_str(b, feed_range);
        wire::write_varint(b, start_ns);
        wire::write_varint(b, duration_ns);
    }

    fn op_end(&mut self, outcome: Outcome, attempt_count: u32, total_ns: u64) {
        self.log.outcome = outcome;
        self.log.attempt_count = attempt_count;
        self.log.total_ns = total_ns;
        let b = &mut self.log.buf;
        b.push(Tag::End as u8);
        b.push(match outcome {
            Outcome::Success => 0,
            Outcome::Error => 1,
        });
        wire::write_varint(b, u64::from(attempt_count));
        wire::write_varint(b, total_ns);
    }
}

/// Collects diagnostics for `input` into a pooled capture log (the hot-path cost: appends only).
pub fn collect(pool: &mut LogPool, input: &OperationInput, clock: &MockClock) -> CaptureLog {
    let buf = pool.rent();
    let mut sink = Combo4Sink::new(buf);
    azure_core_diag_common::drive_sink(input, &mut sink, clock);
    sink.into_log()
}

/// Returns a capture log's buffer to the pool — the "we don't care" path. Effectively free.
pub fn discard(pool: &mut LogPool, log: CaptureLog) {
    pool.give_back(log.into_buf());
}

// ---------------------------------------------------------------------------
// The gate
// ---------------------------------------------------------------------------

/// How aggressively diagnostics are built.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Never build (capture is still cheap; logs are always dropped).
    Off,
    /// Build only when the threshold rule fires (slow or errored).
    Threshold,
    /// Always build.
    Always,
}

/// The policy evaluated at the end of an operation to decide whether to build diagnostics.
#[derive(Clone, Copy, Debug)]
pub struct DiagnosticsPolicy {
    /// Build aggressiveness.
    pub mode: Mode,
    /// Build when the operation took longer than this (nanoseconds). `None` disables the latency gate.
    pub latency_threshold_ns: Option<u64>,
    /// Build when the operation failed.
    pub capture_on_error: bool,
    /// When building, also emit the `AZD1` binary detail blob (opt-in compaction).
    pub binary: bool,
}

impl Default for DiagnosticsPolicy {
    fn default() -> Self {
        // Build on error, or when an op exceeds 5 ms; summary only by default.
        Self {
            mode: Mode::Threshold,
            latency_threshold_ns: Some(5_000_000),
            capture_on_error: true,
            binary: false,
        }
    }
}

/// Evaluates the gate: should we materialize diagnostics for this log?
pub fn should_build(log: &CaptureLog, policy: &DiagnosticsPolicy) -> bool {
    match policy.mode {
        Mode::Off => false,
        Mode::Always => true,
        Mode::Threshold => {
            (policy.capture_on_error && log.outcome == Outcome::Error)
                || policy
                    .latency_threshold_ns
                    .is_some_and(|t| log.total_ns > t)
        }
    }
}

// ---------------------------------------------------------------------------
// Parse + build (only past the gate)
// ---------------------------------------------------------------------------

struct ParsedAttempt {
    index: u32,
    status: u16,
    service_request_id: String,
    request_charge: f32,
    start_ns: u64,
    duration_ns: u64,
}

struct ParsedChild {
    plan_node_id: String,
    feed_range: String,
    start_ns: u64,
    duration_ns: u64,
}

struct Parsed {
    operation: String,
    endpoint: String,
    client_request_id: String,
    start_ns: u64,
    attempts: Vec<ParsedAttempt>,
    children: Vec<ParsedChild>,
    outcome: Outcome,
    attempt_count: u32,
    total_ns: u64,
}

fn parse(buf: &[u8]) -> Parsed {
    let mut p = Parsed {
        operation: String::new(),
        endpoint: String::new(),
        client_request_id: String::new(),
        start_ns: 0,
        attempts: Vec::new(),
        children: Vec::new(),
        outcome: Outcome::Success,
        attempt_count: 0,
        total_ns: 0,
    };
    let mut pos = 0usize;
    while pos < buf.len() {
        let tag = buf[pos];
        pos += 1;
        match tag {
            t if t == Tag::Op as u8 => {
                pos += 1; // preamble id (single global)
                p.operation = wire::read_str(buf, &mut pos).unwrap_or_default();
                p.endpoint = wire::read_str(buf, &mut pos).unwrap_or_default();
                p.client_request_id = wire::read_str(buf, &mut pos).unwrap_or_default();
                p.start_ns = wire::read_varint(buf, &mut pos).unwrap_or(0);
            }
            t if t == Tag::Attempt as u8 => {
                let index = wire::read_varint(buf, &mut pos).unwrap_or(0) as u32;
                let status = wire::read_varint(buf, &mut pos).unwrap_or(0) as u16;
                let service_request_id = wire::read_str(buf, &mut pos).unwrap_or_default();
                let mut ru_bytes = [0u8; 4];
                if let Some(slice) = buf.get(pos..pos + 4) {
                    ru_bytes.copy_from_slice(slice);
                }
                pos += 4;
                let request_charge = f32::from_le_bytes(ru_bytes);
                let start_ns = wire::read_varint(buf, &mut pos).unwrap_or(0);
                let duration_ns = wire::read_varint(buf, &mut pos).unwrap_or(0);
                p.attempts.push(ParsedAttempt {
                    index,
                    status,
                    service_request_id,
                    request_charge,
                    start_ns,
                    duration_ns,
                });
            }
            t if t == Tag::Child as u8 => {
                let plan_node_id = wire::read_str(buf, &mut pos).unwrap_or_default();
                let feed_range = wire::read_str(buf, &mut pos).unwrap_or_default();
                let start_ns = wire::read_varint(buf, &mut pos).unwrap_or(0);
                let duration_ns = wire::read_varint(buf, &mut pos).unwrap_or(0);
                p.children.push(ParsedChild {
                    plan_node_id,
                    feed_range,
                    start_ns,
                    duration_ns,
                });
            }
            t if t == Tag::End as u8 => {
                p.outcome = if buf.get(pos).copied() == Some(1) {
                    Outcome::Error
                } else {
                    Outcome::Success
                };
                pos += 1;
                p.attempt_count = wire::read_varint(buf, &mut pos).unwrap_or(0) as u32;
                p.total_ns = wire::read_varint(buf, &mut pos).unwrap_or(0);
            }
            _ => break,
        }
    }
    p
}

fn error_kind(status: u16) -> Option<&'static str> {
    if (200..300).contains(&status) {
        return None;
    }
    Some(match status {
        429 => "throttled",
        404 => "not_found",
        400..=499 => "client_error",
        500..=599 => "server_error",
        _ => "unknown",
    })
}

/// Compact provenance, rehydrated once from the process-global preamble (cheap to carry, never
/// stored per-attempt).
#[derive(Clone, Debug, Serialize)]
pub struct ClientInfo {
    /// SDK name + version.
    pub sdk_version: String,
    /// Cosmos driver version.
    pub driver_version: String,
    /// Full User-Agent string in the SDK's canonical shape.
    pub user_agent: String,
}

fn client_info() -> ClientInfo {
    let p = preamble::get();
    ClientInfo {
        sdk_version: format!("{} {}", p.sdk_name, p.sdk_version()),
        driver_version: p.driver_version(),
        user_agent: p.user_agent(),
    }
}

/// The default, aggregatable summary view (built only past the gate).
#[derive(Clone, Debug, Serialize)]
pub struct Summary {
    /// Operation name.
    pub operation: String,
    /// `success` / `error`.
    pub outcome: &'static str,
    /// Total attempt count.
    pub attempt_count: u32,
    /// Retries (attempts beyond the first).
    pub retry_count: u32,
    /// Total elapsed time (ns).
    pub total_elapsed_ns: u64,
    /// Summed request charge across attempts (RU).
    pub total_request_charge: f64,
    /// Count of throttled (429) attempts.
    pub throttle_count: u32,
    /// Status-code histogram.
    pub status_counts: BTreeMap<String, u32>,
    /// Service request id of the final attempt.
    pub final_service_request_id: Option<String>,
    /// Number of fan-out children.
    pub child_count: usize,
    /// The first error, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_error: Option<TopError>,
    /// Compact client/version provenance (from the interned preamble).
    pub client: ClientInfo,
}

/// The first error encountered, surfaced in the summary.
#[derive(Clone, Debug, Serialize)]
pub struct TopError {
    /// HTTP status.
    pub status: u16,
    /// Coarse error kind.
    pub error_kind: String,
    /// Service request id of the failing attempt.
    pub service_request_id: String,
}

fn summarize(parsed: &Parsed) -> Summary {
    let mut status_counts: BTreeMap<String, u32> = BTreeMap::new();
    let mut total_request_charge = 0.0f64;
    let mut throttle_count = 0u32;
    let mut top_error: Option<TopError> = None;

    for attempt in &parsed.attempts {
        *status_counts.entry(attempt.status.to_string()).or_insert(0) += 1;
        total_request_charge += f64::from(attempt.request_charge);
        if attempt.status == 429 {
            throttle_count += 1;
        }
        if top_error.is_none() {
            if let Some(kind) = error_kind(attempt.status) {
                top_error = Some(TopError {
                    status: attempt.status,
                    error_kind: kind.to_string(),
                    service_request_id: attempt.service_request_id.clone(),
                });
            }
        }
    }

    Summary {
        operation: parsed.operation.clone(),
        outcome: if parsed.outcome == Outcome::Success {
            "success"
        } else {
            "error"
        },
        attempt_count: parsed.attempt_count,
        retry_count: parsed.attempt_count.saturating_sub(1),
        total_elapsed_ns: parsed.total_ns,
        // RU is stored compactly as f32 on the hot path; round on the way out so the f32→f64
        // widening doesn't leak noise like 8.39999962 into the summary.
        total_request_charge: (total_request_charge * 10_000.0).round() / 10_000.0,
        throttle_count,
        status_counts,
        final_service_request_id: parsed.attempts.last().map(|a| a.service_request_id.clone()),
        child_count: parsed.children.len(),
        top_error,
        client: client_info(),
    }
}

fn build_wiretree(parsed: &Parsed) -> WireTree {
    let info = client_info();
    let mut nodes = Vec::with_capacity(1 + parsed.attempts.len() + parsed.children.len());
    nodes.push(WireNode {
        parent: None,
        kind: NodeKind::Operation as u8,
        start_ns: parsed.start_ns,
        duration_ns: parsed.total_ns,
        status: 0,
        attrs: vec![
            (attrs::ATTR_OPERATION.to_string(), parsed.operation.clone()),
            (attrs::ATTR_ENDPOINT.to_string(), parsed.endpoint.clone()),
            (
                attrs::ATTR_CLIENT_REQUEST_ID.to_string(),
                parsed.client_request_id.clone(),
            ),
            (
                attrs::ATTR_ATTEMPT_COUNT.to_string(),
                parsed.attempt_count.to_string(),
            ),
            (
                "az.outcome".to_string(),
                if parsed.outcome == Outcome::Success {
                    "success".to_string()
                } else {
                    "error".to_string()
                },
            ),
            // Version/UA provenance rehydrated from the interned preamble.
            ("az.sdk_version".to_string(), info.sdk_version),
            ("az.driver_version".to_string(), info.driver_version),
            ("az.user_agent".to_string(), info.user_agent),
        ],
    });
    for attempt in &parsed.attempts {
        let mut node_attrs = vec![
            ("attempt_index".to_string(), attempt.index.to_string()),
            (
                attrs::ATTR_STATUS_CODE.to_string(),
                attempt.status.to_string(),
            ),
            (
                attrs::ATTR_SERVICE_REQUEST_ID.to_string(),
                attempt.service_request_id.clone(),
            ),
            (
                attrs::ATTR_REQUEST_CHARGE.to_string(),
                attempt.request_charge.to_string(),
            ),
        ];
        if let Some(kind) = error_kind(attempt.status) {
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
    for child in &parsed.children {
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
        operation: parsed.operation.clone(),
        nodes,
    }
}

/// The rendered diagnostics for one operation.
#[derive(Clone, Debug)]
pub enum Rendered {
    /// The gate decided the diagnostics were not worth building (success + fast). Nothing emitted.
    Dropped,
    /// Summary-only tier.
    Summary {
        /// Compact summary JSON.
        json: Vec<u8>,
    },
    /// Summary + detailed binary tier.
    Detailed {
        /// Compact summary JSON.
        json: Vec<u8>,
        /// Full span tree as an `AZD1` binary blob.
        blob: Vec<u8>,
    },
}

impl Rendered {
    /// The summary JSON, when one was built.
    pub fn summary_json(&self) -> Option<&[u8]> {
        match self {
            Rendered::Dropped => None,
            Rendered::Summary { json } | Rendered::Detailed { json, .. } => Some(json),
        }
    }

    /// The detailed binary blob, when one was built.
    pub fn detailed_blob(&self) -> Option<&[u8]> {
        match self {
            Rendered::Detailed { blob, .. } => Some(blob),
            _ => None,
        }
    }

    /// Whether the gate dropped the diagnostics.
    pub fn is_dropped(&self) -> bool {
        matches!(self, Rendered::Dropped)
    }
}

/// Serializes a [`Summary`] to compact JSON.
pub fn to_summary_json(summary: &Summary) -> Vec<u8> {
    serde_json::to_vec(summary).expect("Summary serializes")
}

/// Serializes a [`Summary`] to pretty JSON (for samples).
pub fn to_summary_json_pretty(summary: &Summary) -> String {
    serde_json::to_string_pretty(summary).expect("Summary serializes")
}

/// Builds the summary view from a capture log (parse + reduce). Call only past the gate.
pub fn build_summary(log: &CaptureLog) -> Summary {
    summarize(&parse(&log.buf))
}

/// Builds the detailed `AZD1` binary blob from a capture log. Call only past the gate.
pub fn build_detailed_blob(log: &CaptureLog) -> Vec<u8> {
    wire::encode_auto(&build_wiretree(&parse(&log.buf)))
}

/// The full lifecycle: collect → gate → (drop | build), returning a pooled buffer either way.
pub fn capture_and_gate(
    pool: &mut LogPool,
    input: &OperationInput,
    clock: &MockClock,
    policy: &DiagnosticsPolicy,
) -> Rendered {
    let log = collect(pool, input, clock);
    if !should_build(&log, policy) {
        discard(pool, log);
        return Rendered::Dropped;
    }
    let parsed = parse(&log.buf);
    let json = to_summary_json(&summarize(&parsed));
    let rendered = if policy.binary {
        Rendered::Detailed {
            json,
            blob: wire::encode_auto(&build_wiretree(&parsed)),
        }
    } else {
        Rendered::Summary { json }
    };
    discard(pool, log);
    rendered
}

/// Async variant proving response-sourced capture (service request id read from the response).
pub async fn capture_and_gate_via_mock(
    pool: &mut LogPool,
    input: &OperationInput,
    clock: &MockClock,
    policy: &DiagnosticsPolicy,
) -> azure_core::Result<Rendered> {
    let buf = pool.rent();
    let mut sink = Combo4Sink::new(buf);
    azure_core_diag_common::drive_via_mock(input, &mut sink, clock).await?;
    let log = sink.into_log();
    if !should_build(&log, policy) {
        discard(pool, log);
        return Ok(Rendered::Dropped);
    }
    let parsed = parse(&log.buf);
    let json = to_summary_json(&summarize(&parsed));
    let rendered = if policy.binary {
        Rendered::Detailed {
            json,
            blob: wire::encode_auto(&build_wiretree(&parsed)),
        }
    } else {
        Rendered::Summary { json }
    };
    discard(pool, log);
    Ok(rendered)
}

#[cfg(test)]
mod tests {
    use super::*;
    use azure_core_diag_common::scenarios::{s1, s2, s3, s4};
    use azure_core_diag_common::wire::decode;

    fn default_policy() -> DiagnosticsPolicy {
        DiagnosticsPolicy {
            binary: true,
            ..Default::default()
        }
    }

    #[test]
    fn s1_fast_success_is_dropped_for_free() {
        let mut pool = LogPool::new();
        let clock = MockClock::new();
        let rendered = capture_and_gate(&mut pool, &s1(), &clock, &default_policy());
        assert!(rendered.is_dropped(), "fast success should be gated away");
        // The buffer was returned to the pool for reuse.
        assert_eq!(pool.pooled(), 1);
    }

    #[test]
    fn s2_slow_success_builds() {
        let mut pool = LogPool::new();
        let clock = MockClock::new();
        // S2 total is 7ms > 5ms threshold -> build.
        let rendered = capture_and_gate(&mut pool, &s2(), &clock, &default_policy());
        let json = rendered.summary_json().expect("S2 builds a summary");
        let summary: serde_json::Value = serde_json::from_slice(json).unwrap();
        assert_eq!(summary["attempt_count"], 2);
        assert_eq!(summary["throttle_count"], 1);
        assert_eq!(summary["final_service_request_id"], "svc-200");
    }

    #[test]
    fn s3_error_escalates_and_round_trips() {
        let mut pool = LogPool::new();
        let clock = MockClock::new();
        let rendered = capture_and_gate(&mut pool, &s3(), &clock, &default_policy());
        let blob = rendered
            .detailed_blob()
            .expect("error builds detailed binary");
        let tree = decode(blob).unwrap();
        assert_eq!(
            tree.nodes[1].attr(attrs::ATTR_SERVICE_REQUEST_ID),
            Some("svc-404")
        );
        assert_eq!(
            tree.nodes[1].attr(attrs::ATTR_ERROR_KIND),
            Some("not_found")
        );
    }

    #[test]
    fn version_provenance_survives_into_built_output() {
        let mut pool = LogPool::new();
        let clock = MockClock::new();
        let rendered = capture_and_gate(&mut pool, &s3(), &clock, &default_policy());
        let summary: serde_json::Value =
            serde_json::from_slice(rendered.summary_json().unwrap()).unwrap();
        let ua = summary["client"]["user_agent"].as_str().unwrap();
        assert!(ua.starts_with("azsdk-rust-azure_data_cosmos/"));
        // The hot path stored the provenance in a single byte; only the build expands it.
        let blob = rendered.detailed_blob().unwrap();
        let tree = decode(blob).unwrap();
        assert!(tree.nodes[0].attr("az.user_agent").is_some());
    }

    #[test]
    fn off_mode_never_builds_but_still_captures_cheaply() {
        let mut pool = LogPool::new();
        let clock = MockClock::new();
        let policy = DiagnosticsPolicy {
            mode: Mode::Off,
            ..default_policy()
        };
        assert!(capture_and_gate(&mut pool, &s3(), &clock, &policy).is_dropped());
    }

    #[test]
    fn always_mode_builds_even_fast_success() {
        let mut pool = LogPool::new();
        let clock = MockClock::new();
        let policy = DiagnosticsPolicy {
            mode: Mode::Always,
            ..default_policy()
        };
        assert!(!capture_and_gate(&mut pool, &s1(), &clock, &policy).is_dropped());
    }

    #[test]
    fn s4_builds_and_detailed_has_all_children() {
        let mut pool = LogPool::new();
        let clock = MockClock::new();
        let rendered = capture_and_gate(&mut pool, &s4(25), &clock, &default_policy());
        let blob = rendered.detailed_blob().expect("S4 is slow -> builds");
        let tree = decode(blob).unwrap();
        let children = tree
            .nodes
            .iter()
            .filter(|n| n.node_kind() == NodeKind::Routing)
            .count();
        assert_eq!(children, 25);
        // Summary stays compact (a count, not the children).
        let summary: serde_json::Value =
            serde_json::from_slice(rendered.summary_json().unwrap()).unwrap();
        assert_eq!(summary["child_count"], 25);
    }

    #[test]
    fn pool_is_reused_across_operations() {
        let mut pool = LogPool::new();
        for _ in 0..5 {
            let clock = MockClock::new();
            let _ = capture_and_gate(&mut pool, &s1(), &clock, &default_policy());
        }
        // Every op dropped and returned its buffer; the pool never grows past one.
        assert_eq!(pool.pooled(), 1);
    }

    #[tokio::test]
    async fn via_mock_sources_id_from_response() {
        let mut pool = LogPool::new();
        let clock = MockClock::new();
        let rendered = capture_and_gate_via_mock(&mut pool, &s3(), &clock, &default_policy())
            .await
            .unwrap();
        let summary: serde_json::Value =
            serde_json::from_slice(rendered.summary_json().unwrap()).unwrap();
        assert_eq!(summary["top_error"]["service_request_id"], "svc-404");
    }
}
