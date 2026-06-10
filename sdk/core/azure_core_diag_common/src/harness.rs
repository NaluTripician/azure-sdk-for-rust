// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License.

//! Benchmark + sample-dump helpers shared by every combo and the runner.
//!
//! The workspace ships `criterion`, but the scrum deliverable needs one comparison table across
//! combos/scenarios/phases, so we use a small hand-rolled harness that returns structured
//! numbers we can write straight to CSV/Markdown. Each measurement runs a warmup, then many
//! batched samples, and reports mean/median/p95 per operation.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Default warmup samples for a phase measurement.
pub const DEFAULT_WARMUP: usize = 50;
/// Default number of recorded samples per phase.
pub const DEFAULT_SAMPLES: usize = 200;
/// Default inner repetitions per sample (amortizes timer resolution).
pub const DEFAULT_BATCH: usize = 500;

/// Summary statistics for a timed phase, in nanoseconds per operation.
#[derive(Clone, Copy, Debug, Default)]
pub struct Timing {
    /// Arithmetic mean across samples.
    pub mean_ns: f64,
    /// Median (p50) across samples.
    pub median_ns: f64,
    /// 95th percentile across samples.
    pub p95_ns: f64,
}

/// Times `f` with [`DEFAULT_WARMUP`]/[`DEFAULT_SAMPLES`]/[`DEFAULT_BATCH`] settings.
pub fn time_phase<F: FnMut()>(f: F) -> Timing {
    time_phase_with(DEFAULT_WARMUP, DEFAULT_SAMPLES, DEFAULT_BATCH, f)
}

/// Times `f`, running `warmup` discarded samples, then `samples` recorded samples of `batch`
/// inner repetitions each. Returns per-operation statistics.
pub fn time_phase_with<F: FnMut()>(
    warmup: usize,
    samples: usize,
    batch: usize,
    mut f: F,
) -> Timing {
    let batch = batch.max(1);
    for _ in 0..warmup {
        f();
    }
    let mut per_op = Vec::with_capacity(samples.max(1));
    for _ in 0..samples.max(1) {
        let start = Instant::now();
        for _ in 0..batch {
            f();
        }
        let elapsed = start.elapsed().as_nanos() as f64 / batch as f64;
        per_op.push(elapsed);
    }
    summarize(&mut per_op)
}

/// Measures the cost of dropping a freshly built collector by building `batch` collectors
/// outside the timer and timing their drop. Used for the "happy-path discard" metric.
pub fn time_discard<T, B: FnMut() -> T>(samples: usize, batch: usize, mut build: B) -> Timing {
    let batch = batch.max(1);
    let mut per_op = Vec::with_capacity(samples.max(1));
    for _ in 0..samples.max(1) {
        let mut pool: Vec<T> = Vec::with_capacity(batch);
        for _ in 0..batch {
            pool.push(build());
        }
        let start = Instant::now();
        drop(pool);
        let elapsed = start.elapsed().as_nanos() as f64 / batch as f64;
        per_op.push(elapsed);
    }
    summarize(&mut per_op)
}

fn summarize(per_op: &mut [f64]) -> Timing {
    if per_op.is_empty() {
        return Timing::default();
    }
    per_op.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mean = per_op.iter().sum::<f64>() / per_op.len() as f64;
    let median = per_op[per_op.len() / 2];
    let p95_index = (((per_op.len() as f64) * 0.95) as usize).min(per_op.len() - 1);
    Timing {
        mean_ns: mean,
        median_ns: median,
        p95_ns: per_op[p95_index],
    }
}

/// One row of the cross-combo comparison table.
#[derive(Clone, Debug)]
pub struct BenchRow {
    /// Combo name (e.g. `combo3`).
    pub combo: String,
    /// Scenario name (e.g. `S2`).
    pub scenario: String,
    /// Mode (e.g. `default`, `summary`, `detailed`).
    pub mode: String,
    /// Collection time (during the request path), median ns.
    pub collect_ns: f64,
    /// Construction time (finalize/materialize), median ns.
    pub construct_ns: f64,
    /// Serialization time (encode incl. compression), median ns.
    pub serialize_ns: f64,
    /// Decode time (out-of-band), median ns; `0.0` when not applicable.
    pub decode_ns: f64,
    /// Happy-path discard cost (drop on success), median ns; `0.0` when not applicable.
    pub discard_ns: f64,
    /// Output size in bytes.
    pub output_bytes: usize,
}

impl BenchRow {
    /// Total cost paid before the outcome is known plus finalize and serialize — i.e. what an
    /// always-on eager design pays. Useful for happy-path overhead comparisons.
    pub fn full_cost_ns(&self) -> f64 {
        self.collect_ns + self.construct_ns + self.serialize_ns
    }
}

/// Writes the bench rows to a CSV file at `path`.
pub fn write_results_csv(path: impl AsRef<Path>, rows: &[BenchRow]) -> std::io::Result<()> {
    let mut out = String::new();
    out.push_str("combo,scenario,mode,collect_ns,construct_ns,serialize_ns,decode_ns,discard_ns,output_bytes\n");
    for r in rows {
        out.push_str(&format!(
            "{},{},{},{:.1},{:.1},{:.1},{:.1},{:.1},{}\n",
            r.combo,
            r.scenario,
            r.mode,
            r.collect_ns,
            r.construct_ns,
            r.serialize_ns,
            r.decode_ns,
            r.discard_ns,
            r.output_bytes
        ));
    }
    if let Some(parent) = path.as_ref().parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    fs::write(path, out)
}

/// Renders the bench rows as a GitHub-flavored Markdown table.
pub fn results_md_table(rows: &[BenchRow]) -> String {
    let mut out = String::new();
    out.push_str("| Combo | Scenario | Mode | collect ns | construct ns | serialize ns | decode ns | discard ns | bytes |\n");
    out.push_str("|---|---|---|--:|--:|--:|--:|--:|--:|\n");
    for r in rows {
        out.push_str(&format!(
            "| {} | {} | {} | {:.0} | {:.0} | {:.0} | {:.0} | {:.0} | {} |\n",
            r.combo,
            r.scenario,
            r.mode,
            r.collect_ns,
            r.construct_ns,
            r.serialize_ns,
            r.decode_ns,
            r.discard_ns,
            r.output_bytes
        ));
    }
    out
}

/// Returns the base directory for sample dumps.
///
/// Uses `DIAG_SAMPLES_DIR` when set, otherwise `target/diag-samples` relative to the current
/// working directory (so running the runner from the repo root lands files under `target/`).
pub fn samples_dir() -> PathBuf {
    match std::env::var_os("DIAG_SAMPLES_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from("target").join("diag-samples"),
    }
}

/// Writes a sample to `<base>/<combo>/<scenario>.<ext>` and returns the path.
pub fn dump_sample_in(
    base: impl AsRef<Path>,
    combo: &str,
    scenario: &str,
    ext: &str,
    bytes: &[u8],
) -> std::io::Result<PathBuf> {
    let dir = base.as_ref().join(combo);
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{scenario}.{ext}"));
    let mut file = fs::File::create(&path)?;
    file.write_all(bytes)?;
    Ok(path)
}

/// Writes a sample under [`samples_dir`] at `<combo>/<scenario>.<ext>` and returns the path.
pub fn dump_sample(
    combo: &str,
    scenario: &str,
    ext: &str,
    bytes: &[u8],
) -> std::io::Result<PathBuf> {
    dump_sample_in(samples_dir(), combo, scenario, ext, bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_phase_runs_and_reports() {
        let mut counter = 0u64;
        let timing = time_phase_with(2, 5, 10, || {
            counter = counter.wrapping_add(1);
            std::hint::black_box(counter);
        });
        assert!(timing.mean_ns >= 0.0);
        assert!(timing.median_ns >= 0.0);
        assert!(timing.p95_ns >= 0.0);
        // 2 warmup + 5*10 recorded = 52 calls.
        assert_eq!(counter, 52);
    }

    #[test]
    fn dump_sample_writes_a_file() {
        let base = std::env::temp_dir().join(format!("diag-harness-test-{}", std::process::id()));
        let path = dump_sample_in(&base, "combo_test", "S1", "json", b"{\"ok\":true}").unwrap();
        assert!(path.exists());
        let contents = std::fs::read(&path).unwrap();
        assert_eq!(contents, b"{\"ok\":true}");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn csv_and_md_render() {
        let rows = vec![BenchRow {
            combo: "combo3".into(),
            scenario: "S1".into(),
            mode: "default".into(),
            collect_ns: 100.0,
            construct_ns: 50.0,
            serialize_ns: 200.0,
            decode_ns: 0.0,
            discard_ns: 0.0,
            output_bytes: 512,
        }];
        let md = results_md_table(&rows);
        assert!(md.contains("combo3"));
        assert!(md.contains("| 512 |"));

        let base = std::env::temp_dir().join(format!("diag-csv-test-{}", std::process::id()));
        let csv_path = base.join("bench.csv");
        write_results_csv(&csv_path, &rows).unwrap();
        let csv = std::fs::read_to_string(&csv_path).unwrap();
        assert!(csv.starts_with("combo,scenario,mode,"));
        assert!(csv.contains("combo3,S1,default,"));
        let _ = std::fs::remove_dir_all(&base);
    }
}
