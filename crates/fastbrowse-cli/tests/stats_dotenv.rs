//! The stats setting in `.env.example` must work when copied into the working directory's `.env`.

mod common;

use common::isolated_binary;
use serde_json::Value;

fn configured_directory() -> tempfile::TempDir {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join(".env"), "OPENROUTER_API_KEY=test-key\nFASTBROWSE_STATS=runs.jsonl\n")
        .unwrap();
    directory
}

#[test]
fn test_stats_reads_the_log_configured_in_dotenv() {
    let directory = configured_directory();
    let output = isolated_binary().current_dir(directory.path()).args(["stats", "--json"]).output().unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", String::from_utf8_lossy(&output.stderr));
    let summary: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(summary["runs"], 0);
    assert_eq!(summary["file"], "runs.jsonl");
}

#[test]
fn test_a_run_writes_the_log_configured_in_dotenv() {
    let directory = configured_directory();
    // Loading a missing recipe fails inside the runner, before any browser or model connection.
    let output =
        isolated_binary().current_dir(directory.path()).args(["read", "--recipe", "missing.json"]).output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("missing.json"), "{error}");
    let log = directory.path().join("runs.jsonl");
    assert!(log.is_file(), "FASTBROWSE_STATS in .env did not enable logging: {error}");
    let record: Value = serde_json::from_str(std::fs::read_to_string(log).unwrap().trim()).unwrap();
    assert_eq!(record["status"], "error");
}
