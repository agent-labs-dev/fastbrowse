mod common;

use std::io::Write;
use std::sync::{Arc, Mutex};

use common::{result, runner};
use fastbrowse_cli::cli::{self, Context, Streams};
use fastbrowse_clients::environment::Settings;
use fastbrowse_core::models::{RunResult, Status, Wall, WallKind};

#[derive(Clone, Default)]
struct Buffer(Arc<Mutex<Vec<u8>>>);

impl Write for Buffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn a_bot_wall_does_not_tell_the_caller_to_supply_a_password() {
    // The empty-controls needs_login case now returns this wall; advice must follow it too.
    let answer = RunResult {
        wall: Some(Wall {
            kind: WallKind::BotCheck,
            origin: "https://example.test".into(),
            fields: Vec::new(),
            seen_at_ms: 0,
        }),
        error: Some("Access denied".into()),
        ..result(Status::NeedsLogin)
    };
    let output = Buffer::default();
    let context = Context {
        runner: runner(move |_| {
            let answer = answer.clone();
            async move { Ok(answer) }
        }),
        settings: Arc::new(|| Ok(Settings::default())),
        environ: Default::default(),
        streams: Streams { out: Arc::new(Mutex::new(output.clone())), err: Arc::new(Mutex::new(Buffer::default())) },
    };
    let args = ["Read the page", "--start", "https://example.test", "--fetch", "--json"].map(String::from);
    cli::main(&args, &context).await;
    let bytes = output.0.lock().unwrap().clone();
    let result: serde_json::Value = serde_json::from_slice(&bytes).expect("a JSON result");
    assert_eq!(result["wall"]["kind"], "bot_check");
    let error = result["error"].as_str().expect("the refusal");
    assert!(!error.contains("Pass --secret"), "a bot check has no credential field: {error}");
}
