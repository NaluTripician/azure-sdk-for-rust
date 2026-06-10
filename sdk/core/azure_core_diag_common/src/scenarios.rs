// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License.

//! The reference scenarios (S1–S4) and the [`DiagSink`] trait every combo implements.
//!
//! Scenarios are fully scripted, deterministic input. The same [`OperationInput`] is fed to
//! every combo so captured diagnostics and benchmark numbers are directly comparable.
//!
//! Two drivers are provided:
//! * [`drive_sink`] — synchronous; feeds the sink directly from the script. Used by the
//!   benchmark harness so measurements isolate diagnostics cost (no async HTTP overhead).
//! * [`drive_via_mock`] — asynchronous; routes each attempt through an [`MockHttpClient`] and
//!   reads the service request id back out of the **response** `x-ms-request-id` header. Used by
//!   tests to prove real, response-sourced capture on both success and error paths.
//!
//! Both drivers produce identical diagnostics because the mock returns exactly the scripted
//! status, service request id, and request charge.

use crate::attrs;
use crate::clock::MockClock;

/// One scripted HTTP attempt within an operation.
#[derive(Clone, Debug)]
pub struct AttemptScript {
    /// HTTP status code returned by this attempt.
    pub status: u16,
    /// Service request id the mock returns in the `x-ms-request-id` response header.
    pub service_request_id: &'static str,
    /// Request charge (RU) the mock returns in the `x-ms-request-charge` response header.
    pub request_charge: f64,
    /// How long this attempt takes, in nanoseconds (mock clock).
    pub duration_ns: u64,
}

/// One scripted child span (fan-out / routing node), child of the operation root.
#[derive(Clone, Debug)]
pub struct ChildScript {
    /// The query-plan tree node id.
    pub plan_node_id: String,
    /// The feed range this child addresses.
    pub feed_range: String,
    /// How long this child span takes, in nanoseconds (mock clock).
    pub duration_ns: u64,
}

/// A fully-scripted operation: the deterministic input shared by every combo.
#[derive(Clone, Debug)]
pub struct OperationInput {
    /// Operation name (e.g. `read_item`).
    pub name: &'static str,
    /// Service endpoint URL.
    pub endpoint: &'static str,
    /// Client-generated request id.
    pub client_request_id: &'static str,
    /// The HTTP attempts, in order.
    pub attempts: Vec<AttemptScript>,
    /// Fan-out child spans (empty for S1–S3).
    pub children: Vec<ChildScript>,
}

impl OperationInput {
    /// Returns `true` when the final attempt succeeded (HTTP 2xx).
    pub fn succeeded(&self) -> bool {
        self.attempts
            .last()
            .map(|a| (200..300).contains(&a.status))
            .unwrap_or(false)
    }
}

/// The outcome of a driven operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The operation ultimately succeeded.
    Success,
    /// The operation ultimately failed.
    Error,
}

/// The semantic events of an operation. Each combo implements this with its own storage
/// strategy (eager objects, arena, wrapper types); the *inputs* are identical so the designs
/// are compared on equal footing.
pub trait DiagSink {
    /// Called once at the start of the operation.
    fn op_start(&mut self, input: &OperationInput, start_ns: u64);

    /// Called once per HTTP attempt with the values sourced from the response.
    #[allow(clippy::too_many_arguments)]
    fn attempt(
        &mut self,
        attempt_index: u32,
        status: u16,
        service_request_id: Option<&str>,
        request_charge: Option<f64>,
        start_ns: u64,
        duration_ns: u64,
    );

    /// Called once per fan-out child span.
    fn child(
        &mut self,
        child_index: u32,
        plan_node_id: &str,
        feed_range: &str,
        start_ns: u64,
        duration_ns: u64,
    );

    /// Called once at the end of the operation.
    fn op_end(&mut self, outcome: Outcome, attempt_count: u32, total_ns: u64);
}

/// S1 — single success: one attempt, `200`, `svc-200`, RU 4.2.
pub fn s1() -> OperationInput {
    OperationInput {
        name: "read_item",
        endpoint: "https://contoso.documents.azure.com/dbs/db/colls/c/docs/1",
        client_request_id: "client-0001",
        attempts: vec![AttemptScript {
            status: 200,
            service_request_id: "svc-200",
            request_charge: attrs::RU_DEFAULT,
            duration_ns: 4_000_000,
        }],
        children: Vec::new(),
    }
}

/// S2 — retry then success: `429` → `200`, two attempts, `svc-429`/`svc-200`, RU 4.2.
pub fn s2() -> OperationInput {
    OperationInput {
        name: "read_item",
        endpoint: "https://contoso.documents.azure.com/dbs/db/colls/c/docs/1",
        client_request_id: "client-0002",
        attempts: vec![
            AttemptScript {
                status: 429,
                service_request_id: "svc-429",
                request_charge: attrs::RU_DEFAULT,
                duration_ns: 3_000_000,
            },
            AttemptScript {
                status: 200,
                service_request_id: "svc-200",
                request_charge: attrs::RU_DEFAULT,
                duration_ns: 4_000_000,
            },
        ],
        children: Vec::new(),
    }
}

/// S3 — error: `404`, `svc-404`, captured on the error path.
pub fn s3() -> OperationInput {
    OperationInput {
        name: "read_item",
        endpoint: "https://contoso.documents.azure.com/dbs/db/colls/c/docs/missing",
        client_request_id: "client-0003",
        attempts: vec![AttemptScript {
            status: 404,
            service_request_id: "svc-404",
            request_charge: attrs::RU_DEFAULT,
            duration_ns: 2_500_000,
        }],
        children: Vec::new(),
    }
}

/// S4 — fan-out / verbose: a parent query operation with `n` child spans, each carrying a
/// plan-tree node id and feed range. Used to stress size and truncation.
pub fn s4(n: usize) -> OperationInput {
    let children = (0..n)
        .map(|i| ChildScript {
            plan_node_id: format!("plan/node/{i}"),
            feed_range: format!("range-{:02}:[{:04x}-{:04x})", i, i * 256, (i + 1) * 256),
            duration_ns: 1_000_000 + (i as u64) * 50_000,
        })
        .collect();
    OperationInput {
        name: "query_items",
        endpoint: "https://contoso.documents.azure.com/dbs/db/colls/c/docs",
        client_request_id: "client-0004",
        attempts: vec![AttemptScript {
            status: 200,
            service_request_id: "svc-query-200",
            request_charge: 18.6,
            duration_ns: 6_000_000,
        }],
        children,
    }
}

/// Drives `sink` synchronously from the script, advancing `clock` deterministically.
///
/// This is the path the benchmark harness measures: it contains only the diagnostics work, no
/// async HTTP, so collection cost is isolated and comparable across combos.
pub fn drive_sink<S: DiagSink>(input: &OperationInput, sink: &mut S, clock: &MockClock) {
    let op_start = clock.now_ns();
    sink.op_start(input, op_start);

    let mut attempt_count = 0u32;
    for (i, attempt) in input.attempts.iter().enumerate() {
        let start = clock.now_ns();
        clock.advance_ns(attempt.duration_ns);
        sink.attempt(
            i as u32,
            attempt.status,
            Some(attempt.service_request_id),
            Some(attempt.request_charge),
            start,
            attempt.duration_ns,
        );
        attempt_count += 1;
    }

    for (i, child) in input.children.iter().enumerate() {
        let start = clock.now_ns();
        clock.advance_ns(child.duration_ns);
        sink.child(
            i as u32,
            &child.plan_node_id,
            &child.feed_range,
            start,
            child.duration_ns,
        );
    }

    let outcome = if input.succeeded() {
        Outcome::Success
    } else {
        Outcome::Error
    };
    let total = clock.now_ns() - op_start;
    sink.op_end(outcome, attempt_count, total);
}

/// Drives `sink` by routing each attempt through a [`MockHttpClient`], reading the service
/// request id and request charge back out of the **response** headers.
///
/// Proves response-sourced capture on success and error paths. Produces diagnostics identical
/// to [`drive_sink`] because the mock returns exactly the scripted values.
pub async fn drive_via_mock<S: DiagSink>(
    input: &OperationInput,
    sink: &mut S,
    clock: &MockClock,
) -> azure_core::Result<()> {
    use azure_core::http::{
        headers::Headers, request::Request, AsyncRawResponse, HttpClient, Method, StatusCode,
    };
    use azure_core_test::http::MockHttpClient;
    use futures::FutureExt as _;
    use std::sync::{Arc, Mutex};

    let scripts = input.attempts.clone();
    let cursor = Arc::new(Mutex::new(0usize));
    let client = Arc::new(MockHttpClient::new(move |_req| {
        let scripts = scripts.clone();
        let cursor = cursor.clone();
        async move {
            let mut index = cursor.lock().unwrap();
            let attempt = scripts[*index].clone();
            *index += 1;
            drop(index);

            let mut headers = Headers::new();
            headers.insert(attrs::HEADER_SERVICE_REQUEST_ID, attempt.service_request_id);
            headers.insert(
                attrs::HEADER_REQUEST_CHARGE,
                attempt.request_charge.to_string(),
            );
            Ok(AsyncRawResponse::from_bytes(
                StatusCode::from(attempt.status),
                headers,
                Vec::new(),
            ))
        }
        .boxed()
    })) as Arc<dyn HttpClient>;

    let op_start = clock.now_ns();
    sink.op_start(input, op_start);

    let mut attempt_count = 0u32;
    for (i, attempt) in input.attempts.iter().enumerate() {
        let mut request = Request::new(input.endpoint.parse().unwrap(), Method::Get);
        request.insert_header(attrs::HEADER_CLIENT_REQUEST_ID, input.client_request_id);
        let response = client.execute_request(&request).await?;

        // Capture from the response on both success and error paths.
        let status: u16 = response.status().into();
        let service_request_id = response
            .headers()
            .get_optional_str(&attrs::HEADER_SERVICE_REQUEST_ID)
            .map(str::to_string);
        let request_charge = response
            .headers()
            .get_optional_str(&attrs::HEADER_REQUEST_CHARGE)
            .and_then(|v| v.parse::<f64>().ok());

        let start = clock.now_ns();
        clock.advance_ns(attempt.duration_ns);
        sink.attempt(
            i as u32,
            status,
            service_request_id.as_deref(),
            request_charge,
            start,
            attempt.duration_ns,
        );
        attempt_count += 1;
    }

    for (i, child) in input.children.iter().enumerate() {
        let start = clock.now_ns();
        clock.advance_ns(child.duration_ns);
        sink.child(
            i as u32,
            &child.plan_node_id,
            &child.feed_range,
            start,
            child.duration_ns,
        );
    }

    let outcome = if input.succeeded() {
        Outcome::Success
    } else {
        Outcome::Error
    };
    let total = clock.now_ns() - op_start;
    sink.op_end(outcome, attempt_count, total);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A trivial no-op collector that just records the event sequence.
    #[derive(Default)]
    struct CountingSink {
        events: Vec<String>,
        last_outcome: Option<Outcome>,
        attempt_count: u32,
    }

    impl DiagSink for CountingSink {
        fn op_start(&mut self, input: &OperationInput, _start_ns: u64) {
            self.events.push(format!("op_start:{}", input.name));
        }
        fn attempt(
            &mut self,
            attempt_index: u32,
            status: u16,
            service_request_id: Option<&str>,
            _request_charge: Option<f64>,
            _start_ns: u64,
            _duration_ns: u64,
        ) {
            self.events.push(format!(
                "attempt:{attempt_index}:{status}:{}",
                service_request_id.unwrap_or("-")
            ));
        }
        fn child(
            &mut self,
            child_index: u32,
            plan_node_id: &str,
            _feed_range: &str,
            _start_ns: u64,
            _duration_ns: u64,
        ) {
            self.events
                .push(format!("child:{child_index}:{plan_node_id}"));
        }
        fn op_end(&mut self, outcome: Outcome, attempt_count: u32, _total_ns: u64) {
            self.last_outcome = Some(outcome);
            self.attempt_count = attempt_count;
        }
    }

    #[test]
    fn drive_sink_s2_records_both_attempts() {
        let clock = MockClock::new();
        let mut sink = CountingSink::default();
        drive_sink(&s2(), &mut sink, &clock);
        assert_eq!(sink.attempt_count, 2);
        assert_eq!(sink.last_outcome, Some(Outcome::Success));
        assert!(sink.events.iter().any(|e| e == "attempt:0:429:svc-429"));
        assert!(sink.events.iter().any(|e| e == "attempt:1:200:svc-200"));
    }

    #[test]
    fn drive_sink_s4_emits_children() {
        let clock = MockClock::new();
        let mut sink = CountingSink::default();
        drive_sink(&s4(10), &mut sink, &clock);
        let children = sink
            .events
            .iter()
            .filter(|e| e.starts_with("child:"))
            .count();
        assert_eq!(children, 10);
    }

    #[tokio::test]
    async fn drive_via_mock_captures_error_id_from_response() {
        let clock = MockClock::new();
        let mut sink = CountingSink::default();
        drive_via_mock(&s3(), &mut sink, &clock).await.unwrap();
        assert_eq!(sink.last_outcome, Some(Outcome::Error));
        assert!(sink.events.iter().any(|e| e == "attempt:0:404:svc-404"));
    }
}
