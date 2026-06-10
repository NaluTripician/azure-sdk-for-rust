// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License.

//! A deterministic monotonic clock so attempt and total durations are stable in samples.
//!
//! Real wall-clock values would make serialized samples non-reproducible. Every combo reads
//! ticks from a [`MockClock`], and the scenario driver advances it by fixed amounts.

use std::sync::atomic::{AtomicU64, Ordering};

/// A deterministic, monotonically increasing nanosecond clock.
#[derive(Debug)]
pub struct MockClock {
    ticks: AtomicU64,
}

impl MockClock {
    /// A fixed, arbitrary epoch (nanoseconds) so timestamps in samples never depend on the wall clock.
    pub const EPOCH_NS: u64 = 1_700_000_000_000_000_000;

    /// Creates a new clock positioned at [`MockClock::EPOCH_NS`].
    pub fn new() -> Self {
        Self {
            ticks: AtomicU64::new(Self::EPOCH_NS),
        }
    }

    /// Returns the current tick without advancing.
    pub fn now_ns(&self) -> u64 {
        self.ticks.load(Ordering::SeqCst)
    }

    /// Advances the clock by `by` nanoseconds and returns the new value.
    pub fn advance_ns(&self, by: u64) -> u64 {
        self.ticks.fetch_add(by, Ordering::SeqCst) + by
    }

    /// Resets the clock back to [`MockClock::EPOCH_NS`].
    pub fn reset(&self) {
        self.ticks.store(Self::EPOCH_NS, Ordering::SeqCst);
    }
}

impl Default for MockClock {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advances_deterministically() {
        let clock = MockClock::new();
        assert_eq!(clock.now_ns(), MockClock::EPOCH_NS);
        assert_eq!(clock.advance_ns(100), MockClock::EPOCH_NS + 100);
        assert_eq!(clock.now_ns(), MockClock::EPOCH_NS + 100);
        clock.reset();
        assert_eq!(clock.now_ns(), MockClock::EPOCH_NS);
    }
}
