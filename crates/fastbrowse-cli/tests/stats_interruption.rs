//! A CLI run stopped with Ctrl-C must still be counted in its opt-in stats log.

#![cfg(unix)]

mod common;

use std::io::Read;
use std::net::TcpListener;
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use common::isolated_binary;
use serde_json::Value;

struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn test_an_interrupted_run_is_counted_in_stats() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("runs.jsonl");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let mut command = isolated_binary();
    command
        .current_dir(directory.path())
        .args(["read", "--fetch", "--start", &endpoint])
        .env("OPENROUTER_API_KEY", "test-key")
        .env("FASTBROWSE_STATS", &file)
        .env("FASTBROWSE_LLM_BASE_URL", &endpoint)
        .env("FASTBROWSE_LLM_MODEL", "test-model")
        .env("FASTBROWSE_DECISION_BASE_URL", &endpoint)
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut child = Running(command.spawn().unwrap());
    // Hold the first HTTP request open so interruption happens after the runner started, with no external service.
    let deadline = Instant::now() + Duration::from_secs(5);
    let _connection = loop {
        match listener.accept() {
            Ok((connection, _)) => break connection,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(child.0.try_wait().unwrap().is_none(), "the run exited before making its request");
                assert!(Instant::now() < deadline, "the run never made its request");
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("cannot accept the run's request: {error}"),
        }
    };
    // SAFETY: this PID belongs to the live child owned above.
    assert_eq!(unsafe { libc::kill(child.0.id() as libc::pid_t, libc::SIGINT) }, 0);
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "the interrupted process did not exit");
        std::thread::sleep(Duration::from_millis(10));
    };
    let mut stderr = String::new();
    child.0.stderr.take().unwrap().read_to_string(&mut stderr).unwrap();
    assert_eq!(status.code(), Some(130), "{stderr}");
    assert!(file.is_file(), "the interrupted run disappeared from the stats log: {stderr}");
    let text = std::fs::read_to_string(file).unwrap();
    let records: Vec<Value> = text.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["status"], "cancelled");
    assert_eq!(records[0]["cost_complete"], false);
}
