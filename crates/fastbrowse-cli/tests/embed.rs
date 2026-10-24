//! `embed`: a run for a host language in this process answers as `serve` answers the same params.

mod common;

use std::sync::{Arc, Mutex};

use common::{Recorder, browser_event, result, run_params, runner, settings, step_event};
use fastbrowse_cli::embed::{Embedded, Host, HostCall, HostResolve};
use fastbrowse_cli::runner::{Cancel, RunArgs};
use fastbrowse_core::models::Status;
use fastbrowse_core::protocol::ErrorCode;
use serde_json::{Value, json};
use tokio::sync::Notify;

const BROWSER: &str = "ws://127.0.0.1:9222/devtools/browser/x";

fn params(fields: Value) -> String {
    let mut fields = fields;
    fields.as_object_mut().unwrap().insert("cdp_url".into(), BROWSER.into());
    run_params("r", fields).to_string()
}

fn host_call<A: Send + 'static, R: Send + 'static>(
    answer: impl Fn(A) -> Result<R, String> + Send + Sync + 'static,
) -> HostCall<A, R> {
    let answer = Arc::new(answer);
    Arc::new(move |value: A| {
        let answer = answer.clone();
        Box::pin(async move { answer(value) })
    })
}

#[tokio::test]
async fn params_serve_refuses_are_refused_with_its_code_and_message() {
    let embedded = Embedded::with_runner(Recorder::default().runner(), settings());
    let refused = embedded.run(&params(json!({"tsak": 1})), Host::default(), Cancel::default()).await.unwrap_err();
    assert_eq!(refused.code, ErrorCode::InvalidParams);
    assert_eq!(refused.message, "run: tsak: Extra inputs are not permitted");

    let refused = embedded.run(&params(json!({"until": true})), Host::default(), Cancel::default()).await.unwrap_err();
    assert_eq!(refused.message, "run: until is set and no until callback was given");

    let missing = common::settings_with(|settings| {
        settings.openrouter_api_key = None;
        settings.ai_gateway_api_key = None;
    });
    let embedded = Embedded::with_runner(Recorder::default().runner(), missing);
    let local = run_params("r", json!({"chrome": {"binary": "/bin/sh"}, "local": true})).to_string();
    let refused = embedded.run(&local, Host::default(), Cancel::default()).await.unwrap_err();
    assert_eq!(refused.code, ErrorCode::Configuration);
}

#[tokio::test]
async fn events_reach_the_host_as_json_in_order_and_one_it_fails_ends_the_run() {
    let recorder = Recorder { events: vec![browser_event(None, None), step_event(0, None)], ..Recorder::default() };
    let heard = Arc::new(Mutex::new(Vec::new()));
    let kept = heard.clone();
    let on_event = host_call(move |event: Value| {
        kept.lock().unwrap().push(event["type"].clone());
        Ok(())
    });
    let host = Host { on_event: Some(on_event), ..Host::default() };
    let embedded = Embedded::with_runner(recorder.runner(), settings());
    let answered = embedded.run(&params(json!({})), host, Cancel::default()).await.unwrap();
    assert_eq!(answered.status, Status::Complete);
    assert_eq!(*heard.lock().unwrap(), vec![json!("browser"), json!("step")]);

    let held = Recorder { events: vec![browser_event(None, None)], hold: Some(Arc::new(Notify::new())), ..recorder };
    let host = Host { on_event: Some(host_call(|_: Value| Err("boom".to_string()))), ..Host::default() };
    let embedded = Embedded::with_runner(held.runner(), settings());
    let ended = embedded.run(&params(json!({})), host, Cancel::default()).await.unwrap();
    assert_eq!(ended.status, Status::Error);
    assert_eq!(ended.error.as_deref(), Some("run/event: boom"));
}

#[tokio::test]
async fn a_secret_is_asked_only_where_its_ref_reaches_and_a_failure_hides_what_the_host_said() {
    let typist = runner(|args: RunArgs| async move {
        let secrets = args.secrets.clone().expect("a resolver");
        let elsewhere = secrets.resolve("password", "https://evil.example").await;
        let here = secrets.resolve("password", "https://shop.example").await;
        let mut answered = result(Status::Complete);
        answered.data = json!([elsewhere, here]);
        if args.cancel.is_tripped() {
            return Err(fastbrowse_cli::runner::RunError::Cancelled);
        }
        Ok(answered)
    });
    let refs = json!({"secrets": [{"name": "password", "origins": ["https://shop.example"]}]});
    let asked = Arc::new(Mutex::new(Vec::new()));
    let kept = asked.clone();
    let resolve: HostResolve = Arc::new(move |name: String, origin: String| {
        kept.lock().unwrap().push(origin);
        Box::pin(async move { Ok(Some(format!("{name}-value"))) })
    });
    let embedded = Embedded::with_runner(typist.clone(), settings());
    let host = Host { resolve_secret: Some(resolve), ..Host::default() };
    let answered = embedded.run(&params(refs.clone()), host, Cancel::default()).await.unwrap();
    assert_eq!(answered.data, json!([null, "password-value"]));
    assert_eq!(*asked.lock().unwrap(), vec!["https://shop.example".to_string()]);

    let failing: HostResolve = Arc::new(|_, _| Box::pin(async { Err("the vault says hunter2".to_string()) }));
    let host = Host { resolve_secret: Some(failing), ..Host::default() };
    let ended = Embedded::with_runner(typist, settings()).run(&params(refs), host, Cancel::default()).await.unwrap();
    assert_eq!(ended.status, Status::Error);
    assert_eq!(ended.error.as_deref(), Some("secrets/resolve: the client could not resolve the secret 'password'"));
}

#[tokio::test]
async fn a_tripped_cancel_ends_the_run_with_the_cancelled_refusal() {
    let recorder = Recorder { hold: Some(Arc::new(Notify::new())), ..Recorder::default() };
    let embedded = Embedded::with_runner(recorder.runner(), settings());
    let cancel = Cancel::default();
    let stop = cancel.clone();
    tokio::spawn(async move { stop.trip() });
    let refused = embedded.run(&params(json!({})), Host::default(), cancel).await.unwrap_err();
    assert_eq!(refused.code, ErrorCode::Cancelled);
    assert_eq!(refused.message, "run 'r' was cancelled");
}
