// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License.

//! `diag-bench` — benchmark every diagnostics combo on the shared scenarios, emit the comparison
//! CSV, compute size-reduction multipliers, and dump the sample gallery.
//!
//! Run from the repository root so outputs land predictably:
//!
//! ```text
//! cargo run --release -p azure_core_diag_runner --bin diag-bench
//! ```
//!
//! Outputs:
//! * `DIAGNOSTICS-BENCH.csv` (repo root) — one row per combo/scenario/mode.
//! * `target/diag-samples/**` — JSON / binary samples for the gallery.
//! * `target/diag-samples/_bench-table.md`, `_multipliers.md` — rendered fragments for the report.

use std::hint::black_box;
use std::io;

use azure_core_diag_common::harness::{self, time_discard, time_phase_with, BenchRow, Timing};
use azure_core_diag_common::scenarios::{s1, s2, s3, s4, OperationInput};
use azure_core_diag_common::MockClock;

const WARMUP: usize = 20;
const SAMPLES: usize = 100;
const BATCH: usize = 200;

fn bench<F: FnMut()>(f: F) -> Timing {
    time_phase_with(WARMUP, SAMPLES, BATCH, f)
}

fn delta(after: Timing, before: Timing) -> f64 {
    (after.median_ns - before.median_ns).max(0.0)
}

fn scenarios() -> Vec<(&'static str, OperationInput)> {
    vec![
        ("S1", s1()),
        ("S2", s2()),
        ("S3", s3()),
        ("S4x10", s4(10)),
        ("S4x25", s4(25)),
    ]
}

fn bench_combo3(name: &str, input: &OperationInput) -> (BenchRow, Vec<u8>) {
    let t_c = bench(|| {
        let clock = MockClock::new();
        black_box(azure_core_diag_combo3::collect(input, &clock));
    });
    let t_cc = bench(|| {
        let clock = MockClock::new();
        let store = azure_core_diag_combo3::collect(input, &clock);
        black_box(azure_core_diag_combo3::construct(&store));
    });
    let t_ccs = bench(|| {
        let clock = MockClock::new();
        let store = azure_core_diag_combo3::collect(input, &clock);
        let op = azure_core_diag_combo3::construct(&store);
        black_box(azure_core_diag_combo3::to_json_compact(&op));
    });

    let clock = MockClock::new();
    let op = azure_core_diag_combo3::capture(input, &clock);
    let compact = azure_core_diag_combo3::to_json_compact(&op);
    let pretty = azure_core_diag_combo3::to_json_pretty(&op);

    let row = BenchRow {
        combo: "combo3".into(),
        scenario: name.into(),
        mode: "json".into(),
        collect_ns: t_c.median_ns,
        construct_ns: delta(t_cc, t_c),
        serialize_ns: delta(t_ccs, t_cc),
        decode_ns: 0.0,
        discard_ns: 0.0,
        output_bytes: compact.len(),
    };
    (row, pretty.into_bytes())
}

fn bench_combo1(name: &str, input: &OperationInput) -> (BenchRow, Vec<u8>) {
    let t_c = bench(|| {
        let clock = MockClock::new();
        black_box(azure_core_diag_combo1::collect(input, &clock));
    });
    let t_cc = bench(|| {
        let clock = MockClock::new();
        let arena = azure_core_diag_combo1::collect(input, &clock);
        black_box(azure_core_diag_combo1::construct(&arena));
    });
    let t_ccs = bench(|| {
        let clock = MockClock::new();
        let arena = azure_core_diag_combo1::collect(input, &clock);
        let wire = azure_core_diag_combo1::construct(&arena);
        black_box(azure_core_diag_combo1::serialize(&wire));
    });

    let clock = MockClock::new();
    let arena = azure_core_diag_combo1::collect(input, &clock);
    let wire = azure_core_diag_combo1::construct(&arena);
    let blob = azure_core_diag_combo1::serialize(&wire);

    let t_dec = bench(|| {
        black_box(azure_core_diag_combo1::decode_blob(&blob).unwrap());
    });
    let t_disc = time_discard(SAMPLES, BATCH, || {
        let clock = MockClock::new();
        azure_core_diag_combo1::collect(input, &clock)
    });

    let row = BenchRow {
        combo: "combo1".into(),
        scenario: name.into(),
        mode: "binary".into(),
        collect_ns: t_c.median_ns,
        construct_ns: delta(t_cc, t_c),
        serialize_ns: delta(t_ccs, t_cc),
        decode_ns: t_dec.median_ns,
        discard_ns: t_disc.median_ns,
        output_bytes: blob.len(),
    };
    (row, blob)
}

struct Combo2Out {
    summary_row: BenchRow,
    detailed_row: BenchRow,
    summary_pretty: Vec<u8>,
    detailed_blob: Vec<u8>,
}

fn bench_combo2(name: &str, input: &OperationInput) -> Combo2Out {
    use azure_core_diag_combo2 as c2;
    let thresholds = c2::SummaryThresholds::default();

    // Shared collect (hot path) cost.
    let t_c = bench(|| {
        let clock = MockClock::new();
        black_box(c2::collect(input, &clock));
    });

    // Summary tier.
    let t_cc_sum = bench(|| {
        let clock = MockClock::new();
        let cap = c2::collect(input, &clock);
        black_box(c2::summarize(&cap, &thresholds));
    });
    let t_ccs_sum = bench(|| {
        let clock = MockClock::new();
        let cap = c2::collect(input, &clock);
        let summary = c2::summarize(&cap, &thresholds);
        black_box(c2::to_summary_json(&summary));
    });
    let t_disc = time_discard(SAMPLES, BATCH, || {
        let clock = MockClock::new();
        c2::collect(input, &clock)
    });

    // Detailed tier.
    let t_cc_det = bench(|| {
        let clock = MockClock::new();
        let cap = c2::collect(input, &clock);
        black_box(c2::detailed_wire(&cap));
    });
    let t_ccs_det = bench(|| {
        let clock = MockClock::new();
        let cap = c2::collect(input, &clock);
        black_box(c2::detailed_blob(&cap));
    });

    let clock = MockClock::new();
    let cap = c2::collect(input, &clock);
    let summary = c2::summarize(&cap, &thresholds);
    let summary_json = c2::to_summary_json(&summary);
    let summary_pretty = c2::to_summary_json_pretty(&summary).into_bytes();
    let detailed_blob = c2::detailed_blob(&cap);

    let t_dec = bench(|| {
        black_box(azure_core_diag_common::decode(&detailed_blob).unwrap());
    });

    let summary_row = BenchRow {
        combo: "combo2".into(),
        scenario: name.into(),
        mode: "summary".into(),
        collect_ns: t_c.median_ns,
        construct_ns: delta(t_cc_sum, t_c),
        serialize_ns: delta(t_ccs_sum, t_cc_sum),
        decode_ns: 0.0,
        discard_ns: t_disc.median_ns,
        output_bytes: summary_json.len(),
    };
    let detailed_row = BenchRow {
        combo: "combo2".into(),
        scenario: name.into(),
        mode: "detailed".into(),
        collect_ns: t_c.median_ns,
        construct_ns: delta(t_cc_det, t_c),
        serialize_ns: delta(t_ccs_det, t_cc_det),
        decode_ns: t_dec.median_ns,
        discard_ns: 0.0,
        output_bytes: detailed_blob.len(),
    };

    Combo2Out {
        summary_row,
        detailed_row,
        summary_pretty,
        detailed_blob,
    }
}

fn multipliers_md(rows: &[BenchRow]) -> String {
    use std::collections::BTreeMap;
    // base = combo3 json bytes per scenario.
    let mut base: BTreeMap<&str, usize> = BTreeMap::new();
    for r in rows {
        if r.combo == "combo3" {
            base.insert(r.scenario.as_str(), r.output_bytes);
        }
    }
    let mut out = String::new();
    out.push_str("## Output size & reduction vs Combo 3 JSON\n\n");
    out.push_str("| Scenario | Combo 3 JSON (bytes) | Combo 1 binary | Combo 2 summary | Combo 2 detailed | C1 × smaller | C2-summary × smaller |\n");
    out.push_str("|---|--:|--:|--:|--:|--:|--:|\n");
    for (scenario, &b3) in &base {
        let find = |combo: &str, mode: &str| {
            rows.iter()
                .find(|r| r.scenario == *scenario && r.combo == combo && r.mode == mode)
                .map(|r| r.output_bytes)
                .unwrap_or(0)
        };
        let c1 = find("combo1", "binary");
        let c2s = find("combo2", "summary");
        let c2d = find("combo2", "detailed");
        let mult_c1 = if c1 > 0 { b3 as f64 / c1 as f64 } else { 0.0 };
        let mult_c2s = if c2s > 0 { b3 as f64 / c2s as f64 } else { 0.0 };
        out.push_str(&format!(
            "| {scenario} | {b3} | {c1} | {c2s} | {c2d} | {mult_c1:.1}× | {mult_c2s:.1}× |\n"
        ));
    }
    out
}

fn happy_path_md(rows: &[BenchRow]) -> String {
    // S1 is a single success: compare always-on eager (combo3) vs drop-on-success (combo1) vs
    // cheap summary (combo2 summary).
    let find = |combo: &str, mode: &str| {
        rows.iter()
            .find(|r| r.scenario == "S1" && r.combo == combo && r.mode == mode)
            .cloned()
    };
    let mut out = String::new();
    out.push_str("\n## Happy-path overhead (S1, success)\n\n");
    out.push_str("On success, Combo 1 drops the arena without serializing, and Combo 2 (summary) ");
    out.push_str("never builds the full tree. Combo 3 pays collect + construct + serialize ");
    out.push_str("unconditionally.\n\n");
    out.push_str("| Design | Happy-path cost (ns, median) | Notes |\n|---|--:|---|\n");
    if let Some(r) = find("combo3", "json") {
        out.push_str(&format!(
            "| Combo 3 (eager) | {:.0} | collect+construct+serialize, always |\n",
            r.full_cost_ns()
        ));
    }
    if let Some(r) = find("combo1", "binary") {
        out.push_str(&format!(
            "| Combo 1 (arena, drop on success) | {:.0} | collect + free drop ({:.0} ns); no serialize |\n",
            r.collect_ns + r.discard_ns,
            r.discard_ns
        ));
    }
    if let Some(r) = find("combo2", "summary") {
        out.push_str(&format!(
            "| Combo 2 (summary default) | {:.0} | collect+reduce+summary JSON |\n",
            r.full_cost_ns()
        ));
    }
    out
}

fn main() -> io::Result<()> {
    if cfg!(debug_assertions) {
        eprintln!(
            "warning: running a debug build; numbers are only meaningful in --release. \
             Re-run with: cargo run --release -p azure_core_diag_runner --bin diag-bench"
        );
    }
    eprintln!(
        "environment: os={} arch={} profile={}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        }
    );

    let mut rows: Vec<BenchRow> = Vec::new();
    let base = harness::samples_dir();

    for (name, input) in scenarios() {
        // Combo 3 (baseline JSON).
        let (row3, pretty3) = bench_combo3(name, &input);
        harness::dump_sample_in(&base, "combo3", name, "json", &pretty3)?;
        rows.push(row3);

        // Combo 1 (binary).
        let (row1, blob1) = bench_combo1(name, &input);
        harness::dump_sample_in(&base, "combo1", name, "bin", &blob1)?;
        let decoded1 = azure_core_diag_combo1::decode_blob(&blob1)
            .map(|t| serde_json::to_string_pretty(&t).unwrap())
            .unwrap_or_else(|e| format!("decode error: {e}"));
        harness::dump_sample_in(&base, "combo1", name, "decoded.json", decoded1.as_bytes())?;
        rows.push(row1);

        // Combo 2 (tiered).
        let c2 = bench_combo2(name, &input);
        harness::dump_sample_in(&base, "combo2", name, "summary.json", &c2.summary_pretty)?;
        harness::dump_sample_in(&base, "combo2", name, "detailed.bin", &c2.detailed_blob)?;
        let decoded2 = azure_core_diag_common::decode(&c2.detailed_blob)
            .map(|t| serde_json::to_string_pretty(&t).unwrap())
            .unwrap_or_else(|e| format!("decode error: {e}"));
        harness::dump_sample_in(
            &base,
            "combo2",
            name,
            "detailed.decoded.json",
            decoded2.as_bytes(),
        )?;
        rows.push(c2.summary_row);
        rows.push(c2.detailed_row);
    }

    // Write the CSV at the repo root.
    harness::write_results_csv("DIAGNOSTICS-BENCH.csv", &rows)?;

    // Write rendered fragments for the report.
    let table = harness::results_md_table(&rows);
    let multipliers = multipliers_md(&rows);
    let happy = happy_path_md(&rows);
    std::fs::write(base.join("_bench-table.md"), &table)?;
    std::fs::write(
        base.join("_multipliers.md"),
        format!("{multipliers}{happy}"),
    )?;

    println!("\n=== Diagnostics bench ===\n");
    println!("{table}");
    println!("{multipliers}{happy}");
    println!(
        "Wrote DIAGNOSTICS-BENCH.csv ({} rows) and samples under {}",
        rows.len(),
        base.display()
    );
    Ok(())
}
