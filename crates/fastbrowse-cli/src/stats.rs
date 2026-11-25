//! An opt-in local log of what each run cost and how long it took, and the summary `fastbrowse stats` reads back.
//!
//! The log records nothing about the page, the task or any secret: a status, a step count, a duration, a cost and when
//! the run started, one JSON object per line. It is off unless `FASTBROWSE_STATS` names a file, so a default install
//! writes nothing. `--json` and a trace already carry far more per run; this is the small always-on history you can
//! read p50/p95 from without keeping every trace.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fastbrowse_core::models::RunResult;
use serde_json::{Value, json};

/// The variable that turns the log on: the JSON Lines file it appends to, one object per run.
pub const STATS_ENV: &str = "FASTBROWSE_STATS";

/// The file the log is written to, or None when nothing asked for one.
pub fn destination(environ: &HashMap<String, String>) -> Option<PathBuf> {
    environ.get(STATS_ENV).map(|value| value.trim()).filter(|value| !value.is_empty()).map(PathBuf::from)
}

/// Append one line for `result`. A log that cannot be written is reported to stderr and never fails the run: the run's
/// own answer is what the caller asked for.
pub fn record(environ: &HashMap<String, String>, result: &RunResult, elapsed: Duration) {
    let Some(path) = destination(environ) else { return };
    if let Err(error) = append(&path, &line(result, elapsed)) {
        eprintln!("fastbrowse: could not write the run stats to {}: {error}", path.display());
    }
}

/// The object one run adds to the log. No task text, address or secret ever appears in it.
fn line(result: &RunResult, elapsed: Duration) -> Value {
    json!({
        "at_ms": now_ms(),
        "status": result.status.as_str(),
        "steps": result.steps.len(),
        "duration_ms": elapsed.as_millis() as u64,
        "dollars": fastbrowse_core::py::round(result.cost.known_dollars(), 6),
        "cost_complete": !result.cost.has_unknown(),
    })
}

fn append(path: &Path, record: &Value) -> std::io::Result<()> {
    if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(file, "{record}")
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| since.as_millis() as u64)
}

/// The summary of a log: how many runs, the median and 95th-percentile duration, the error rate and the mean priced
/// cost. Built from the objects a log holds, so the arithmetic is done once here rather than in a shell.
#[derive(Debug, Default, PartialEq)]
pub struct Summary {
    pub runs: usize,
    /// Runs that reached `complete`; every other status counts as an error for the rate.
    pub complete: usize,
    pub p50_ms: u64,
    pub p95_ms: u64,
    /// The mean cost over the runs whose cost was fully known, and how many that was.
    pub mean_dollars: Option<f64>,
    pub priced: usize,
}

impl Summary {
    pub fn of(records: &[Value]) -> Summary {
        let mut summary = Summary { runs: records.len(), ..Summary::default() };
        let mut durations: Vec<u64> = Vec::with_capacity(records.len());
        let mut dollars = 0.0;
        for record in records {
            durations.push(record["duration_ms"].as_u64().unwrap_or(0));
            if record["status"].as_str() == Some("complete") {
                summary.complete += 1;
            }
            if record["cost_complete"].as_bool() == Some(true)
                && let Some(value) = record["dollars"].as_f64()
            {
                dollars += value;
                summary.priced += 1;
            }
        }
        durations.sort_unstable();
        summary.p50_ms = percentile(&durations, 0.50);
        summary.p95_ms = percentile(&durations, 0.95);
        if summary.priced > 0 {
            summary.mean_dollars = Some(dollars / summary.priced as f64);
        }
        summary
    }

    /// The error rate as a fraction of runs, where a run that did not reach `complete` is an error.
    pub fn error_rate(&self) -> f64 {
        if self.runs == 0 { 0.0 } else { (self.runs - self.complete) as f64 / self.runs as f64 }
    }

    /// The human summary a person reads, in the same words whichever entry point printed it.
    pub fn render(&self, file: &Path) -> String {
        let mut out = format!(
            "runs: {} ({} complete, {:.0}% error)\nduration: p50 {} ms, p95 {} ms\n",
            self.runs,
            self.complete,
            self.error_rate() * 100.0,
            self.p50_ms,
            self.p95_ms
        );
        match self.mean_dollars {
            Some(mean) => out.push_str(&format!("cost: mean ${mean:.4} over {} priced runs\n", self.priced)),
            None => out.push_str("cost: no run reported a complete cost\n"),
        }
        out.push_str(&format!("file: {}\n", file.display()));
        out
    }
}

/// Nearest-rank percentile over sorted values; 0 for an empty list.
fn percentile(sorted: &[u64], fraction: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = (fraction * sorted.len() as f64).ceil().max(1.0) as usize;
    sorted[rank.min(sorted.len()) - 1]
}

/// `fastbrowse stats [--file PATH] [--json]`: the summary of the log, or of the file named.
pub fn command(argv: &[String], environ: &HashMap<String, String>) -> (String, i32) {
    let mut file: Option<PathBuf> = None;
    let mut as_json = false;
    let mut args = argv.iter().map(String::as_str);
    while let Some(arg) = args.next() {
        match arg {
            "--json" => as_json = true,
            "--file" => match args.next() {
                Some(value) => file = Some(PathBuf::from(value)),
                None => return ("fastbrowse stats: --file needs a path\n".into(), 2),
            },
            "-h" | "--help" => return (USAGE.into(), 0),
            other => return (format!("fastbrowse stats: unknown argument {other}\n{USAGE}"), 2),
        }
    }
    let path = match file.or_else(|| destination(environ)) {
        Some(path) => path,
        None => return ("fastbrowse stats: no log to read; set FASTBROWSE_STATS or pass --file\n".into(), 2),
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        // A log that was never written yet is not an error: it is simply empty.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return (format!("fastbrowse stats: cannot read {}: {error}\n", path.display()), 1),
    };
    let records: Vec<Value> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    let summary = Summary::of(&records);
    if as_json {
        let body = json!({
            "runs": summary.runs,
            "complete": summary.complete,
            "error_rate": summary.error_rate(),
            "p50_ms": summary.p50_ms,
            "p95_ms": summary.p95_ms,
            "mean_dollars": summary.mean_dollars,
            "priced": summary.priced,
            "file": path.display().to_string(),
        });
        return (format!("{}\n", serde_json::to_string_pretty(&body).unwrap_or_default()), 0);
    }
    (summary.render(&path), 0)
}

pub const USAGE: &str =
    "usage: fastbrowse stats [--file PATH] [--json]\n\nSummarise the runs in the local stats log.\n";

#[cfg(test)]
mod tests {
    use super::*;

    fn at(duration: u64, status: &str, dollars: f64, complete: bool) -> Value {
        json!({
            "at_ms": 0,
            "status": status,
            "steps": 2,
            "duration_ms": duration,
            "dollars": dollars,
            "cost_complete": complete,
        })
    }

    #[test]
    fn the_summary_reports_median_percentile_and_error_rate() {
        let records: Vec<Value> =
            (1..=100).map(|n| at(n * 10, if n % 4 == 0 { "error" } else { "complete" }, 0.01, true)).collect();
        let summary = Summary::of(&records);
        assert_eq!(summary.runs, 100);
        assert_eq!(summary.complete, 75);
        assert!((summary.error_rate() - 0.25).abs() < 1e-9);
        assert_eq!(summary.p50_ms, 500);
        assert_eq!(summary.p95_ms, 950);
        assert_eq!(summary.priced, 100);
        assert!((summary.mean_dollars.unwrap() - 0.01).abs() < 1e-9);
    }

    #[test]
    fn a_run_with_no_reported_cost_is_left_out_of_the_mean() {
        let records = vec![at(100, "complete", 0.02, true), at(200, "complete", 9.99, false)];
        let summary = Summary::of(&records);
        assert_eq!(summary.priced, 1);
        assert_eq!(summary.mean_dollars, Some(0.02));
        assert_eq!(summary.p50_ms, 100);
    }

    #[test]
    fn an_empty_log_summarises_to_zero_without_dividing_by_it() {
        let summary = Summary::of(&[]);
        assert_eq!(summary.runs, 0);
        assert_eq!(summary.error_rate(), 0.0);
        assert_eq!(summary.mean_dollars, None);
        assert_eq!(percentile(&[], 0.5), 0);
    }

    #[test]
    fn a_written_line_carries_no_task_text_and_is_read_back() {
        let dir = std::env::temp_dir().join(format!("fb-stats-{}", std::process::id()));
        let file = dir.join("runs.jsonl");
        let mut environ = HashMap::new();
        environ.insert(STATS_ENV.to_string(), file.display().to_string());
        let result = fastbrowse_core::models::RunResult {
            status: fastbrowse_core::models::Status::Complete,
            budget: None,
            answer: Some("42".into()),
            data: Value::Null,
            evidence: Vec::new(),
            steps: Vec::new(),
            cost: fastbrowse_core::models::CostBreakdown { lines: Vec::new() },
            artifacts: Vec::new(),
            citations: Vec::new(),
            error: None,
            failure_kind: None,
            final_url: None,
            final_frame: None,
            recordings: Vec::new(),
            wall: None,
            browser: None,
        };
        record(&environ, &result, Duration::from_millis(1234));
        let text = std::fs::read_to_string(&file).unwrap();
        let value: Value = serde_json::from_str(text.trim()).unwrap();
        assert_eq!(value["status"], "complete");
        assert_eq!(value["duration_ms"], 1234);
        assert!(value.get("task").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
