// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License.

//! Process-global **diagnostics preamble**: the SDK/driver version + User-Agent suffix.
//!
//! These are constant for the lifetime of the process, so they must never be stored per-attempt
//! or per-operation. The capture log records a single 1-byte [`PREAMBLE_ID`] referencing this
//! table; the full strings are only rehydrated at *build* time (and only when the gate says we
//! actually want the diagnostics).
//!
//! This mirrors what .NET does: Azure.Core builds the User-Agent once
//! (`azsdk-net-<pkg>/<ver> (<runtime>; <os>)`) and Cosmos's `UserAgentContainer` appends a feature
//! suffix; diagnostics record it once in the summary, not per request. The Rust SDK's own
//! User-Agent has the same shape — `azsdk-rust-<crate>/<ver> (<rustc>; <os>; <arch>)`
//! (see `azure_core` `user_agent.rs`) — so this preamble is the diagnostics-side analog.

use std::sync::OnceLock;

/// The id every capture log uses to reference the single process-global preamble.
pub const PREAMBLE_ID: u8 = 0;

/// The constant version/User-Agent provenance for this process.
#[derive(Clone, Debug)]
pub struct Preamble {
    /// SDK crate name.
    pub sdk_name: &'static str,
    /// SDK version packed as `[major, minor, patch]` (varint-friendly, not an ASCII string).
    pub sdk_ver: [u16; 3],
    /// Cosmos driver version packed as `[major, minor, patch]`.
    pub driver_ver: [u16; 3],
    /// Optional User-Agent feature suffix (e.g. enabled-feature flags).
    pub ua_suffix: &'static str,
    /// Target OS.
    pub os: &'static str,
    /// Target architecture.
    pub arch: &'static str,
}

impl Preamble {
    /// Renders the SDK version as `major.minor.patch`.
    pub fn sdk_version(&self) -> String {
        format!(
            "{}.{}.{}",
            self.sdk_ver[0], self.sdk_ver[1], self.sdk_ver[2]
        )
    }

    /// Renders the driver version as `major.minor.patch`.
    pub fn driver_version(&self) -> String {
        format!(
            "{}.{}.{}",
            self.driver_ver[0], self.driver_ver[1], self.driver_ver[2]
        )
    }

    /// Rehydrates the full User-Agent string in the SDK's canonical shape.
    pub fn user_agent(&self) -> String {
        let base = format!(
            "azsdk-rust-{}/{} ({}; {})",
            self.sdk_name,
            self.sdk_version(),
            self.os,
            self.arch
        );
        if self.ua_suffix.is_empty() {
            base
        } else {
            format!("{base} {}", self.ua_suffix)
        }
    }
}

/// Returns the process-global preamble, building it once on first use.
pub fn get() -> &'static Preamble {
    static PREAMBLE: OnceLock<Preamble> = OnceLock::new();
    PREAMBLE.get_or_init(|| Preamble {
        sdk_name: "azure_data_cosmos",
        sdk_ver: [0, 1, 0],
        driver_ver: [0, 1, 0],
        ua_suffix: "",
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
    })
}
