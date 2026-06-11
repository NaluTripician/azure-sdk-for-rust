// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License.

//! The op-end **gate** and its policy.
//!
//! After an operation completes, [`DiagnosticsPolicy`] decides whether the diagnostics are worth
//! building. On a fast success the recorder's buffer is returned to the pool for ~free; on a slow
//! or errored operation the log is parsed once and reduced to a summary (and, opt-in, an `AZD1`
//! blob).
//!
//! The latency + request-charge thresholds bridge to the driver's existing
//! [`DiagnosticsThresholds`](crate::options::DiagnosticsThresholds) via
//! [`DiagnosticsPolicy::from_thresholds`], so the gate reuses the dimensions Cosmos already
//! thresholds on rather than inventing new ones.

use super::recorder::{parse, DiagnosticsRecorder};
use super::summary::{build_detailed_blob, summarize, Summary};
use super::{OperationKind, Outcome};
use crate::options::DiagnosticsThresholds;
use std::time::Duration;

/// How aggressively diagnostics are built at the gate.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    /// Never build. Capture is skipped entirely — truly zero cost.
    #[default]
    Off,
    /// Build only when the threshold rule fires (slow, or errored when `capture_on_error`).
    Threshold,
    /// Always build.
    Always,
}

/// The policy evaluated at the end of an operation to decide whether to build diagnostics.
///
/// The default is [`Mode::Off`] (no cost, no behavior change) — diagnostics are opt-in. Use
/// [`DiagnosticsPolicy::from_thresholds`] to derive a threshold policy from the driver's
/// [`DiagnosticsThresholds`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiagnosticsPolicy {
    /// Build aggressiveness.
    pub mode: Mode,
    /// Build when the operation took longer than this. `None` disables the latency gate.
    pub latency_threshold: Option<Duration>,
    /// Build when the operation failed.
    pub capture_on_error: bool,
    /// When building, also emit the `AZD1` binary detail blob (opt-in compaction).
    pub binary: bool,
}

impl Default for DiagnosticsPolicy {
    fn default() -> Self {
        // Opt-in: off by default, so an unconfigured client pays nothing and behaves unchanged.
        Self {
            mode: Mode::Off,
            latency_threshold: None,
            capture_on_error: true,
            binary: false,
        }
    }
}

impl DiagnosticsPolicy {
    /// A threshold policy that builds on error or when an operation exceeds `latency_threshold`.
    ///
    /// Summary-only (binary off); flip [`DiagnosticsPolicy::binary`] to also emit the `AZD1` blob.
    pub fn threshold(latency_threshold: Duration) -> Self {
        Self {
            mode: Mode::Threshold,
            latency_threshold: Some(latency_threshold),
            capture_on_error: true,
            binary: false,
        }
    }

    /// Always build (summary-only by default).
    pub fn always() -> Self {
        Self {
            mode: Mode::Always,
            latency_threshold: None,
            capture_on_error: true,
            binary: false,
        }
    }

    /// Builds a threshold policy from the driver's [`DiagnosticsThresholds`] for an operation kind.
    ///
    /// The point vs non-point latency threshold is chosen by `kind`. The request-charge and
    /// payload-size thresholds in `DiagnosticsThresholds` are not part of the latency gate; they
    /// are surfaced in the summary instead (a future enhancement may add charge-gated build).
    /// The resulting policy is always [`Mode::Threshold`] with `capture_on_error = true`; callers
    /// can override `mode`/`binary` afterwards.
    pub fn from_thresholds(thresholds: &DiagnosticsThresholds, kind: OperationKind) -> Self {
        let latency = match kind {
            OperationKind::Point => thresholds.point_operation_latency_threshold(),
            OperationKind::NonPoint => thresholds.non_point_operation_latency_threshold(),
        };
        Self {
            mode: Mode::Threshold,
            latency_threshold: latency,
            capture_on_error: true,
            binary: false,
        }
    }
}

/// The rendered diagnostics for one operation.
#[derive(Clone, Debug)]
pub enum Rendered {
    /// The gate decided the diagnostics were not worth building (fast success). Nothing emitted.
    Dropped,
    /// Summary-only tier.
    Summary {
        /// The aggregatable summary.
        summary: Box<Summary>,
    },
    /// Summary + detailed binary tier.
    Detailed {
        /// The aggregatable summary.
        summary: Box<Summary>,
        /// Full span tree as an `AZD1` binary blob.
        blob: Vec<u8>,
    },
}

impl Rendered {
    /// The summary, when one was built.
    pub fn summary(&self) -> Option<&Summary> {
        match self {
            Rendered::Dropped => None,
            Rendered::Summary { summary } | Rendered::Detailed { summary, .. } => Some(summary),
        }
    }

    /// The detailed `AZD1` binary blob, when one was built.
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

/// Evaluates the gate against a recorder's recorded outcome and elapsed time.
pub fn should_build(outcome: Outcome, total_ns: u64, policy: &DiagnosticsPolicy) -> bool {
    match policy.mode {
        Mode::Off => false,
        Mode::Always => true,
        Mode::Threshold => {
            (policy.capture_on_error && outcome == Outcome::Error)
                || policy
                    .latency_threshold
                    .is_some_and(|t| u128::from(total_ns) > t.as_nanos())
        }
    }
}

/// Applies the gate to a finished recorder: drop cheaply, or build the summary (and opt-in blob).
///
/// Either way the recorder's buffer is returned to the pool. Call after
/// [`DiagnosticsRecorder::record_end`].
pub fn finish(recorder: DiagnosticsRecorder, policy: &DiagnosticsPolicy) -> Rendered {
    if !should_build(recorder.outcome(), recorder.total_ns(), policy) {
        recorder.return_buffer();
        return Rendered::Dropped;
    }
    let parsed = parse(recorder.bytes());
    let summary = Box::new(summarize(&parsed));
    let rendered = if policy.binary {
        Rendered::Detailed {
            summary,
            blob: build_detailed_blob(&parsed),
        }
    } else {
        Rendered::Summary { summary }
    };
    recorder.return_buffer();
    rendered
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_never_builds() {
        let p = DiagnosticsPolicy::default();
        assert_eq!(p.mode, Mode::Off);
        assert!(!should_build(Outcome::Error, u64::MAX, &p));
        assert!(!should_build(Outcome::Success, 0, &p));
    }

    #[test]
    fn always_always_builds() {
        let p = DiagnosticsPolicy::always();
        assert!(should_build(Outcome::Success, 0, &p));
        assert!(should_build(Outcome::Error, 0, &p));
    }

    #[test]
    fn threshold_builds_on_slow_or_error() {
        let p = DiagnosticsPolicy::threshold(Duration::from_millis(5));
        // fast success -> drop
        assert!(!should_build(Outcome::Success, 1_000_000, &p));
        // slow success -> build
        assert!(should_build(Outcome::Success, 6_000_000, &p));
        // fast error -> build (capture_on_error)
        assert!(should_build(Outcome::Error, 1_000_000, &p));
    }

    #[test]
    fn threshold_without_error_capture_only_gates_on_latency() {
        let p = DiagnosticsPolicy {
            capture_on_error: false,
            ..DiagnosticsPolicy::threshold(Duration::from_millis(5))
        };
        assert!(!should_build(Outcome::Error, 1_000_000, &p));
        assert!(should_build(Outcome::Error, 6_000_000, &p));
    }

    #[test]
    fn from_thresholds_picks_point_vs_non_point() {
        let thresholds = DiagnosticsThresholds::new()
            .with_point_operation_latency_threshold(Duration::from_millis(1))
            .with_non_point_operation_latency_threshold(Duration::from_millis(100));
        let point = DiagnosticsPolicy::from_thresholds(&thresholds, OperationKind::Point);
        let non_point = DiagnosticsPolicy::from_thresholds(&thresholds, OperationKind::NonPoint);
        assert_eq!(point.latency_threshold, Some(Duration::from_millis(1)));
        assert_eq!(
            non_point.latency_threshold,
            Some(Duration::from_millis(100))
        );
    }
}
