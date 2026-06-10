// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License.

//! `diag-decode` — decode an `AZD1` diagnostics binary blob into human-readable JSON.
//!
//! Usage:
//! ```text
//! diag-decode <blob-file>     # decode the file, print pretty JSON to stdout
//! diag-decode                 # read the blob from stdin
//! ```
//!
//! This is the D4 decode tool. It is the *real* decoder the sample gallery runs, so the
//! "decoded JSON" shown in the report is genuine output, not a hand-written approximation.

use std::io::{self, Read, Write};
use std::process::ExitCode;

fn run() -> Result<(), String> {
    let arg = std::env::args().nth(1);
    let blob = match arg.as_deref() {
        Some("-h") | Some("--help") => {
            print_usage();
            return Ok(());
        }
        Some(path) => std::fs::read(path).map_err(|e| format!("reading {path}: {e}"))?,
        None => {
            let mut buf = Vec::new();
            io::stdin()
                .read_to_end(&mut buf)
                .map_err(|e| format!("reading stdin: {e}"))?;
            buf
        }
    };

    let tree = azure_core_diag_common::wire::decode(&blob).map_err(|e| e.to_string())?;
    let json = serde_json::to_string_pretty(&tree).map_err(|e| e.to_string())?;
    let mut stdout = io::stdout().lock();
    stdout
        .write_all(json.as_bytes())
        .and_then(|_| stdout.write_all(b"\n"))
        .map_err(|e| format!("writing stdout: {e}"))?;
    Ok(())
}

fn print_usage() {
    eprintln!("diag-decode <blob-file>   decode AZD1 blob file to JSON");
    eprintln!("diag-decode               decode AZD1 blob from stdin to JSON");
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("diag-decode: {err}");
            ExitCode::FAILURE
        }
    }
}
