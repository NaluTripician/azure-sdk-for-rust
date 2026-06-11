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

struct Combo4Out {
    rows: Vec<BenchRow>,
    summary_pretty: Vec<u8>,
    detailed_blob: Vec<u8>,
    /// Whether the example policy built (true) or dropped (false) this scenario.
    built: bool,
}

fn bench_combo4(name: &str, input: &OperationInput) -> Combo4Out {
    use azure_core_diag_combo4 as c4;
    // Example policy: build on error, or when an op exceeds 5 ms; binary opt-in.
    let policy = c4::DiagnosticsPolicy {
        mode: c4::Mode::Threshold,
        latency_threshold_ns: Some(5_000_000),
        capture_on_error: true,
        binary: true,
    };

    // Pool reused across iterations so the "drop" path measures rent+append+return.
    // collect+discard cycle (the dropped / happy-path cost).
    let mut pool_a = c4::LogPool::new();
    let t_drop = bench(|| {
        let clock = MockClock::new();
        let log = c4::collect(&mut pool_a, input, &clock);
        c4::discard(&mut pool_a, log);
    });
    // rent+return only (no append) -> isolates pool overhead so collect = t_drop - t_noop.
    let mut pool_b = c4::LogPool::new();
    let t_noop = bench(|| {
        let clock = MockClock::new();
        let log = c4::collect(&mut pool_b, &noop_input(), &clock);
        c4::discard(&mut pool_b, log);
    });
    let collect_ns = (t_drop.median_ns - t_noop.median_ns).max(0.0);
    let discard_ns = t_noop.median_ns;

    // Build-summary cost (parse + reduce + json), measured as a delta over the drop cycle.
    let mut pool_c = c4::LogPool::new();
    let t_sum = bench(|| {
        let clock = MockClock::new();
        let log = c4::collect(&mut pool_c, input, &clock);
        black_box(c4::build_summary(&log));
        c4::discard(&mut pool_c, log);
    });
    let mut pool_cj = c4::LogPool::new();
    let t_sum_json = bench(|| {
        let clock = MockClock::new();
        let log = c4::collect(&mut pool_cj, input, &clock);
        let s = c4::build_summary(&log);
        black_box(c4::to_summary_json(&s));
        c4::discard(&mut pool_cj, log);
    });
    let sum_construct = (t_sum.median_ns - t_drop.median_ns).max(0.0);
    let sum_serialize = (t_sum_json.median_ns - t_sum.median_ns).max(0.0);

    // Build-detailed cost (parse + wiretree + encode).
    let mut pool_d = c4::LogPool::new();
    let t_det = bench(|| {
        let clock = MockClock::new();
        let log = c4::collect(&mut pool_d, input, &clock);
        black_box(c4::build_detailed_blob(&log));
        c4::discard(&mut pool_d, log);
    });
    let det_build = (t_det.median_ns - t_drop.median_ns).max(0.0);

    // Concrete artifacts under the example policy.
    let mut pool = c4::LogPool::new();
    let clock = MockClock::new();
    let rendered = c4::capture_and_gate(&mut pool, input, &clock, &policy);
    let built = !rendered.is_dropped();

    let clock = MockClock::new();
    let log = c4::collect(&mut pool, input, &clock);
    let summary = c4::build_summary(&log);
    let summary_pretty = c4::to_summary_json_pretty(&summary).into_bytes();
    let summary_json = c4::to_summary_json(&summary);
    let detailed_blob = c4::build_detailed_blob(&log);
    c4::discard(&mut pool, log);

    let t_dec = bench(|| {
        black_box(azure_core_diag_common::decode(&detailed_blob).unwrap());
    });

    let rows = vec![
        // The gated-away path: what an op pays when we decide we don't want diagnostics.
        BenchRow {
            combo: "combo4".into(),
            scenario: name.into(),
            mode: "dropped".into(),
            collect_ns,
            construct_ns: 0.0,
            serialize_ns: 0.0,
            decode_ns: 0.0,
            discard_ns,
            output_bytes: 0,
        },
        BenchRow {
            combo: "combo4".into(),
            scenario: name.into(),
            mode: "summary".into(),
            collect_ns,
            construct_ns: sum_construct,
            serialize_ns: sum_serialize,
            decode_ns: 0.0,
            discard_ns: 0.0,
            output_bytes: summary_json.len(),
        },
        BenchRow {
            combo: "combo4".into(),
            scenario: name.into(),
            mode: "detailed".into(),
            collect_ns,
            construct_ns: det_build,
            serialize_ns: 0.0,
            decode_ns: t_dec.median_ns,
            discard_ns: 0.0,
            output_bytes: detailed_blob.len(),
        },
    ];

    Combo4Out {
        rows,
        summary_pretty,
        detailed_blob,
        built,
    }
}

/// A trivial input used to isolate pool rent/return overhead from the append work.
fn noop_input() -> OperationInput {
    OperationInput {
        name: "noop",
        endpoint: "https://noop",
        client_request_id: "noop",
        attempts: Vec::new(),
        children: Vec::new(),
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
    if let Some(r) = find("combo4", "dropped") {
        out.push_str(&format!(
            "| Combo 4 (gated, dropped) | {:.0} | collect ({:.0}) + return to pool ({:.0}); **built nothing** |\n",
            r.collect_ns + r.discard_ns,
            r.collect_ns,
            r.discard_ns
        ));
    }
    out
}

/// Shows the gate deciding per scenario under the example policy, with the dropped vs built costs.
fn gate_md(rows: &[BenchRow], decisions: &[(&str, bool)]) -> String {
    let find = |scenario: &str, mode: &str| {
        rows.iter()
            .find(|r| r.scenario == scenario && r.combo == "combo4" && r.mode == mode)
            .cloned()
    };
    let mut out = String::new();
    out.push_str("\n## Combo 4 — the gate in action (policy: build on error OR > 5 ms)\n\n");
    out.push_str(
        "Hot-path `collect` is paid always; everything else only when the gate says build.\n\n",
    );
    out.push_str("| Scenario | Gate decision | collect ns | if dropped: +discard ns | if built: +construct+serialize ns | summary bytes | detailed bytes |\n");
    out.push_str("|---|---|--:|--:|--:|--:|--:|\n");
    for (scenario, built) in decisions {
        let dropped = find(scenario, "dropped");
        let summary = find(scenario, "summary");
        let detailed = find(scenario, "detailed");
        let collect = dropped.as_ref().map(|r| r.collect_ns).unwrap_or(0.0);
        let discard = dropped.as_ref().map(|r| r.discard_ns).unwrap_or(0.0);
        let build = summary
            .as_ref()
            .map(|r| r.construct_ns + r.serialize_ns)
            .unwrap_or(0.0);
        let sum_bytes = summary.as_ref().map(|r| r.output_bytes).unwrap_or(0);
        let det_bytes = detailed.as_ref().map(|r| r.output_bytes).unwrap_or(0);
        let decision = if *built { "**build**" } else { "drop (free)" };
        out.push_str(&format!(
            "| {scenario} | {decision} | {collect:.0} | {discard:.0} | {build:.0} | {sum_bytes} | {det_bytes} |\n"
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
    let mut gate_decisions: Vec<(&str, bool)> = Vec::new();
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

        // Combo 4 (deferred, threshold-gated capture).
        let c4 = bench_combo4(name, &input);
        harness::dump_sample_in(&base, "combo4", name, "summary.json", &c4.summary_pretty)?;
        harness::dump_sample_in(&base, "combo4", name, "detailed.bin", &c4.detailed_blob)?;
        let decoded4 = azure_core_diag_common::decode(&c4.detailed_blob)
            .map(|t| serde_json::to_string_pretty(&t).unwrap())
            .unwrap_or_else(|e| format!("decode error: {e}"));
        harness::dump_sample_in(
            &base,
            "combo4",
            name,
            "detailed.decoded.json",
            decoded4.as_bytes(),
        )?;
        gate_decisions.push((name, c4.built));
        rows.extend(c4.rows);
    }

    // Write the CSV at the repo root.
    harness::write_results_csv("DIAGNOSTICS-BENCH.csv", &rows)?;

    // Write rendered fragments for the report.
    let table = harness::results_md_table(&rows);
    let multipliers = multipliers_md(&rows);
    let happy = happy_path_md(&rows);
    let gate = gate_md(&rows, &gate_decisions);
    std::fs::write(base.join("_bench-table.md"), &table)?;
    std::fs::write(
        base.join("_multipliers.md"),
        format!("{multipliers}{happy}{gate}"),
    )?;

    println!("\n=== Diagnostics bench ===\n");
    println!("{table}");
    println!("{multipliers}{happy}{gate}");
    println!(
        "Wrote DIAGNOSTICS-BENCH.csv ({} rows) and samples under {}",
        rows.len(),
        base.display()
    );
    Ok(())
}
