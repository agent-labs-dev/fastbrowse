//! Drive the browser crate through scripted steps and print what each step produced, for the differential check
//! against Python's `CdpPage` (bench/differential.py) and for timing it (bench/run.py).
//!
//! Usage: fastbrowse-probe <script.json> <main-origin> [--only <page>]
//!
//! The script lists pages, each a fixture path and steps: `observe`, `capture`, or an action naming its target by
//! label (the first control in the latest observation with that label, optionally the nth). Every page runs in a
//! fresh session on one launched Chrome, as each Python browser test does. Prints `{"pages": [...]}` with each
//! step's output and its wall time in milliseconds.

use std::env;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Instant;

use fastbrowse_browser::adapters::local_chrome::LocalChromeProcess;
use fastbrowse_browser::page::CdpPage;
use fastbrowse_browser::session::{BrowserSession, SessionOptions};
use fastbrowse_browser::shared::{
    Action, Artifact, ArtifactKind, ArtifactSink, BoxError, BoxFuture, BrowserConnection, BrowserError, Config,
    LocalChrome, Observation, Operation,
};
use serde_json::{Value, json};

/// Downloads are dropped: the check compares what the page reports, and a capture of a download's text.
struct NullSink;

impl ArtifactSink for NullSink {
    fn put<'a>(
        &'a self,
        kind: ArtifactKind,
        name: &'a str,
        mime_type: &'a str,
        content: Vec<u8>,
    ) -> BoxFuture<'a, Result<Artifact, BoxError>> {
        Box::pin(async move {
            Ok(Artifact {
                kind,
                name: name.to_owned(),
                mime_type: mime_type.to_owned(),
                size_bytes: content.len() as u64,
                sha256: String::new(),
                uri: format!("memory://{name}"),
            })
        })
    }
}

fn error_json(error: &BrowserError) -> Value {
    json!({"error": error.kind(), "message": error.to_string()})
}

fn operation(name: &str) -> Option<Operation> {
    serde_json::from_value(Value::String(name.to_owned())).ok()
}

/// The control a step names: `label` (and `nth`, from 0) among the latest observation's controls.
fn target(observation: &Observation, step: &Value, key: &str) -> Option<String> {
    let label = step[key].as_str()?;
    let nth = step[format!("{key}_nth")].as_u64().unwrap_or(0) as usize;
    observation.controls.iter().filter(|c| c.label == label).nth(nth).map(|c| c.id.clone())
}

async fn run_step(page: &CdpPage, last: &mut Option<Observation>, base: &str, step: &Value) -> Value {
    let op = step["op"].as_str().unwrap_or_default();
    match op {
        "observe" => match page.observe().await {
            Ok(observation) => {
                let out = serde_json::to_value(&observation).unwrap_or(Value::Null);
                *last = Some(observation);
                out
            }
            Err(error) => error_json(&error),
        },
        "capture" => match page.capture().await {
            Ok(capture) => serde_json::to_value(&capture).unwrap_or(Value::Null),
            Err(error) => error_json(&error),
        },
        "goto" => match page.navigate(&format!("{base}{}", step["path"].as_str().unwrap_or("/")), None).await {
            Ok(()) => json!({"ok": true}),
            Err(error) => error_json(&error),
        },
        "sleep" => {
            tokio::time::sleep(std::time::Duration::from_secs_f64(step["seconds"].as_f64().unwrap_or(0.0))).await;
            json!({"ok": true})
        }
        name => {
            let Some(operation) = operation(name) else { return json!({"error": "UnknownStep", "message": name}) };
            let Some(observation) = last.as_ref() else { return json!({"error": "NoObservation"}) };
            let mut action = Action::new(operation);
            if step.get("label").is_some() {
                match target(observation, step, "label") {
                    Some(id) => action.target_id = Some(id),
                    None => return json!({"error": "NoSuchControl", "message": step["label"]}),
                }
            }
            if step.get("to").is_some() {
                action.destination_id = target(observation, step, "to");
            }
            action.text = step["text"].as_str().map(str::to_owned);
            action.accept_dialog = step["accept"].as_bool();
            action.url = step["path"].as_str().map(|p| format!("{base}{p}"));
            if let Some(index) = step["tab"].as_u64() {
                action.tab_id = observation.tabs.get(index as usize).map(|t| t.id.clone());
            }
            match page.act(&action, observation).await {
                Ok(result) => serde_json::to_value(&result).unwrap_or(Value::Null),
                Err(error) => error_json(&error),
            }
        }
    }
}

async fn run_page(connection: &BrowserConnection, base: &str, spec: &Value) -> Value {
    let session = BrowserSession::new(connection.clone(), Arc::new(NullSink), SessionOptions::default());
    if let Err(error) = session.open().await {
        return json!({"name": spec["name"], "error": error_json(&error)});
    }
    let page = CdpPage::new(session.clone(), Config::default());
    let mut steps = vec![];
    let started = Instant::now();
    let goto = page.navigate(&format!("{base}{}", spec["path"].as_str().unwrap_or("/")), None).await;
    let navigate_ms = started.elapsed().as_secs_f64() * 1000.0;
    if let Err(error) = goto {
        steps.push(json!({"op": "navigate", "ms": navigate_ms, "out": error_json(&error)}));
    } else {
        steps.push(json!({"op": "navigate", "ms": navigate_ms, "out": {"ok": true}}));
        let mut last = None;
        for step in spec["steps"].as_array().into_iter().flatten() {
            let started = Instant::now();
            let out = run_step(&page, &mut last, base, step).await;
            steps.push(json!({"op": step["op"], "ms": started.elapsed().as_secs_f64() * 1000.0, "out": out}));
        }
    }
    let _ = session.close().await;
    json!({"name": spec["name"], "steps": steps})
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let (Some(script), Some(base)) = (args.first(), args.get(1)) else {
        eprintln!("usage: fastbrowse-probe <script.json> <main-origin> [--only <page>]");
        return ExitCode::from(2);
    };
    let only = args.iter().position(|a| a == "--only").and_then(|i| args.get(i + 1));
    let script: Value = match std::fs::read_to_string(script).map(|text| serde_json::from_str(&text)) {
        Ok(Ok(script)) => script,
        _ => {
            eprintln!("fastbrowse-probe: cannot read {script}");
            return ExitCode::from(2);
        }
    };
    let started = Instant::now();
    let (mut chrome, connection) = match LocalChromeProcess::launch(&LocalChrome::default()).await {
        Ok(launched) => launched,
        Err(error) => {
            eprintln!("fastbrowse-probe: {error}");
            return ExitCode::from(1);
        }
    };
    let launch_ms = started.elapsed().as_secs_f64() * 1000.0;
    let mut pages = vec![];
    for spec in script["pages"].as_array().into_iter().flatten() {
        if only.is_some_and(|only| spec["name"].as_str() != Some(only)) {
            continue;
        }
        pages.push(run_page(&connection, base, spec).await);
    }
    chrome.close().await;
    println!("{}", json!({"launch_ms": launch_ms, "pages": pages}));
    ExitCode::SUCCESS
}
