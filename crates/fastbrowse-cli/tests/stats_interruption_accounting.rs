//! Interrupted runs retain steps already reported by the runner.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{Recorder, settings, step_event};
use fastbrowse_cli::cli::{self, Context, Streams};
use serde_json::Value;
use tokio::sync::Notify;

#[tokio::test]
async fn test_interrupted_stats_preserve_reported_steps() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("runs.jsonl");
    let recorder =
        Recorder { events: vec![step_event(1, None)], hold: Some(Arc::new(Notify::new())), ..Recorder::default() };
    let context = Context {
        runner: recorder.runner(),
        settings: settings(),
        environ: [("FASTBROWSE_STATS".into(), file.display().to_string()), ("FASTBROWSE_TRACE".into(), "0".into())]
            .into(),
        streams: Streams { out: Arc::new(Mutex::new(std::io::sink())), err: Arc::new(Mutex::new(std::io::sink())) },
        build: Default::default(),
        kept_at: Default::default(),
    };
    // Dropping the CLI future is the interruption path used by entry::until_interrupted.
    assert!(
        tokio::time::timeout(Duration::from_millis(50), cli::main(&["read".into(), "--fetch".into()], &context))
            .await
            .is_err()
    );
    assert_eq!(recorder.calls().len(), 1);
    let text = std::fs::read_to_string(file).unwrap();
    let record: Value = serde_json::from_str(text.trim()).unwrap();
    assert_eq!(record["status"], "cancelled");
    assert_eq!(record["cost_complete"], false);
    assert_eq!(record["steps"], 1, "the CLI received a completed step before interruption");
}
