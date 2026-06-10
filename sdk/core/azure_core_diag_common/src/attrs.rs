// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License.

//! Canonical diagnostics attribute keys and the HTTP header names the scenarios use.
//!
//! These are shared across every combo so the captured attributes are directly comparable.
//! The service request id always comes from the **response** header `x-ms-request-id`,
//! never from the request.

use azure_core::http::headers::HeaderName;

/// Response header carrying the service-generated request id (`x-ms-request-id`).
pub const HEADER_SERVICE_REQUEST_ID: HeaderName = HeaderName::from_static("x-ms-request-id");

/// Header carrying the client-generated request id (`x-ms-client-request-id`).
pub const HEADER_CLIENT_REQUEST_ID: HeaderName = HeaderName::from_static("x-ms-client-request-id");

/// Response header carrying the Cosmos request charge in Request Units (`x-ms-request-charge`).
pub const HEADER_REQUEST_CHARGE: HeaderName = HeaderName::from_static("x-ms-request-charge");

/// Canonical attribute key for the service request id.
pub const ATTR_SERVICE_REQUEST_ID: &str = "az.service_request_id";
/// Canonical attribute key for the client request id.
pub const ATTR_CLIENT_REQUEST_ID: &str = "az.client_request_id";
/// Canonical attribute key for the HTTP status code.
pub const ATTR_STATUS_CODE: &str = "az.status_code";
/// Canonical attribute key for the error kind on the error path.
pub const ATTR_ERROR_KIND: &str = "az.error_kind";
/// Canonical attribute key for the request charge (RU).
pub const ATTR_REQUEST_CHARGE: &str = "az.request_charge";
/// Canonical attribute key for the service endpoint.
pub const ATTR_ENDPOINT: &str = "az.endpoint";
/// Canonical attribute key for the query plan tree node id (fan-out / routing).
pub const ATTR_PLAN_NODE_ID: &str = "az.plan_node_id";
/// Canonical attribute key for the feed range a child span addresses.
pub const ATTR_FEED_RANGE: &str = "az.feed_range";
/// Canonical attribute key for the total attempt count of an operation.
pub const ATTR_ATTEMPT_COUNT: &str = "az.attempt_count";
/// Canonical attribute key for the operation name.
pub const ATTR_OPERATION: &str = "az.operation";

/// Default request charge (RU) used by the reference scenarios.
pub const RU_DEFAULT: f64 = 4.2;
