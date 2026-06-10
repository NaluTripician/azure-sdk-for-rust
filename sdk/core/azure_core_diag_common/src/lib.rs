// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License.

//! Shared scaffolding for the Azure SDK Rust diagnostics design spikes.
//!
//! This crate is **not published**. It exists so each combo prototype
//! ([`azure_core_diag_combo3`](https://github.com/azure/azure-sdk-for-rust),
//! `..._combo1`, `..._combo2`) is benchmarked and sampled on identical, deterministic input.
//!
//! Modules:
//! * [`attrs`] — canonical attribute keys + HTTP header names.
//! * [`clock`] — a deterministic [`clock::MockClock`].
//! * [`scenarios`] — the S1–S4 reference scenarios, the [`scenarios::DiagSink`] trait, and the
//!   synchronous + mock-HTTP drivers.
//! * [`wire`] — the shared [`wire::WireTree`] model and the `AZD1` binary codec.
//! * [`harness`] — the timing harness, sample dumper, and CSV/Markdown writers.

pub mod attrs;
pub mod clock;
pub mod harness;
pub mod scenarios;
pub mod wire;

pub use clock::MockClock;
pub use scenarios::{
    drive_sink, drive_via_mock, s1, s2, s3, s4, AttemptScript, ChildScript, DiagSink,
    OperationInput, Outcome,
};
pub use wire::{decode, encode, encode_auto, DecodeError, NodeKind, WireNode, WireTree};
