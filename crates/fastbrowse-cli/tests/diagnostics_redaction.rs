//! Diagnostics use the same credential masking as the other binary entry points.

mod common;

use common::isolated_binary;

const ADDRESS: &str = "https://ops:diagnostic-password@host.test/runs.jsonl";

fn assert_masked(output: std::process::Output) {
    let text = format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    assert!(!text.contains("diagnostic-password"), "credential escaped: {text}");
}

#[test]
fn test_stats_masks_quoted_arguments() {
    assert_masked(isolated_binary().args(["stats", ADDRESS]).output().unwrap());
}

#[test]
fn test_doctor_masks_reported_environment_paths() {
    assert_masked(
        isolated_binary()
            .arg("doctor")
            .env("TMPDIR", ADDRESS)
            .env("TMP", ADDRESS)
            .env("TEMP", ADDRESS)
            .output()
            .unwrap(),
    );
}

#[test]
fn test_stats_write_warning_masks_its_destination() {
    let directory = tempfile::tempdir().unwrap();
    // Prevent the URL-shaped destination from becoming a writable relative directory.
    std::fs::write(directory.path().join("https:"), "not a directory").unwrap();
    assert_masked(
        isolated_binary()
            .current_dir(directory.path())
            .args(["read", "--recipe", "missing.json"])
            .env("OPENROUTER_API_KEY", "test-key")
            .env("FASTBROWSE_STATS", ADDRESS)
            .output()
            .unwrap(),
    );
}
