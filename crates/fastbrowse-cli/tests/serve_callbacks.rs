//! tests/test_serve_callbacks.py: frames, the `until` check and a failing client callback over the serve protocol.

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use common::{browser_event, result, run_params, runner, serving, settings};
use fastbrowse_cli::runner::{RunArgs, RunError, Runner};
use fastbrowse_core::models::{RunResult, SecretResolver, Status};
use fastbrowse_core::py::base64;
use serde_json::{Value, json};

// Two JPEG headers with bytes after them that are not text, so a frame written as anything but base64 fails.
const FRAMES: [&[u8]; 2] = [b"\xff\xd8\xff\xe0\x00\x10JFIF\x00\xfb\xff", b"\xff\xd8\xff\xe1\x00\x00\xfe\xfa"];
const ORDERS: &str = "https://shop.example.com/orders/1042";

/// Stands where `run_task` does: it draws its frames, then finishes on ORDERS, `complete` only when the `until`
/// check it was given accepts that address.
#[derive(Clone, Default)]
struct Browsing {
    frames: bool,
    got_frame_handler: Arc<Mutex<Option<bool>>>,
    got_until: Arc<Mutex<Option<bool>>>,
    /// Whether the run has let go of its browser, which a real one does on every way out.
    closed: Arc<AtomicBool>,
}

impl Browsing {
    fn runner(&self) -> Arc<dyn Runner> {
        let browsing = self.clone();
        runner(move |args: RunArgs| {
            let browsing = browsing.clone();
            async move {
                *browsing.got_frame_handler.lock().unwrap() = Some(args.on_frame.is_some());
                *browsing.got_until.lock().unwrap() = Some(args.until.is_some());
                let outcome = async {
                    (args.on_event.as_ref().unwrap())(browser_event(None, None)).await;
                    if browsing.frames {
                        for frame in FRAMES {
                            (args.on_frame.as_ref().unwrap())(frame.to_vec()).await;
                        }
                    }
                    let accepted = match &args.until {
                        None => true,
                        Some(until) => args.cancel.guard(until(ORDERS.into())).await?,
                    };
                    if args.cancel.is_tripped() {
                        return Err(RunError::Cancelled);
                    }
                    let status = if accepted { Status::Complete } else { Status::Unverified };
                    Ok(RunResult { final_url: Some(ORDERS.into()), ..result(status) })
                }
                .await;
                browsing.closed.store(true, Ordering::SeqCst);
                outcome
            }
        })
    }
}

#[tokio::test]
async fn test_frames_arrive_as_notifications_carrying_the_run_id_when_the_run_asks_for_them() {
    let browsing = Browsing { frames: true, ..Browsing::default() };
    let mut client = serving(browsing.runner(), settings());
    let response = client.request("run", Some(run_params("run-7", json!({"frames": true})))).await;
    assert_eq!(response["result"]["status"], "complete");
    let sent: Vec<&Value> = client.inbox.iter().filter(|m| m["method"] == "run/frame").collect();
    assert!(sent.iter().all(|message| message.get("id").is_none()));
    assert!(sent.iter().all(|message| message["params"]["run_id"] == "run-7"));
    let frames: Vec<Vec<u8>> =
        sent.iter().map(|message| base64::decode(message["params"]["frame"].as_str().unwrap()).unwrap()).collect();
    assert_eq!(frames, FRAMES.map(<[u8]>::to_vec));
}

#[tokio::test]
async fn test_a_run_that_does_not_ask_for_frames_gets_no_frame_handler_and_no_frame_is_written() {
    let browsing = Browsing::default();
    let mut client = serving(browsing.runner(), settings());
    let response = client.request("run", Some(run_params("run-1", json!({})))).await;
    assert_eq!(response["result"]["status"], "complete");
    assert_eq!(*browsing.got_frame_handler.lock().unwrap(), Some(false));
    assert_eq!(client.inbox.iter().map(|m| m["method"].clone()).collect::<Vec<_>>(), [json!("run/event")]);
}

#[tokio::test]
async fn test_a_client_that_holds_a_check_is_asked_with_the_final_url_and_its_yes_lets_the_run_complete() {
    let mut client = serving(Browsing::default().runner(), settings());
    let run = client.call("run", Some(run_params("run-7", json!({"until": true}))));
    let asked = client.asked("run/until").await;
    client.respond(&asked, json!(true));
    let reply = client.reply(run).await;
    assert_eq!(
        asked,
        json!({"jsonrpc": "2.0", "id": 1, "method": "run/until", "params": {"run_id": "run-7", "url": ORDERS}})
    );
    assert_eq!(reply["result"]["status"], "complete");
}

#[tokio::test]
async fn test_a_no_from_the_clients_check_keeps_the_run_from_complete() {
    let mut client = serving(Browsing::default().runner(), settings());
    let run = client.call("run", Some(run_params("run-1", json!({"until": true}))));
    let asked = client.asked("run/until").await;
    client.respond(&asked, json!(false));
    assert_eq!(client.reply(run).await["result"]["status"], "unverified");
}

#[tokio::test]
async fn test_a_run_whose_client_holds_no_check_gets_none_and_is_not_asked() {
    let browsing = Browsing::default();
    let mut client = serving(browsing.runner(), settings());
    let response = client.request("run", Some(run_params("run-1", json!({"until": false})))).await;
    assert_eq!(response["result"]["status"], "complete");
    assert_eq!(*browsing.got_until.lock().unwrap(), Some(false));
    assert_eq!(client.inbox.iter().map(|m| m["method"].clone()).collect::<Vec<_>>(), [json!("run/event")]);
}

/// A run that a client's callback ended, apart from its `error`: nothing the run did before then is in it.
fn errored(error: &str) -> Value {
    json!({
        "status": "error", "budget": null, "answer": null, "data": null, "evidence": [], "steps": [],
        "cost": {"lines": []}, "artifacts": [], "citations": [], "error": error, "final_url": null,
        "would_fire": [], "recordings": [], "wall": null,
    })
}

#[tokio::test]
async fn test_an_error_reply_to_the_check_ends_the_run_as_an_error_result_after_its_browser_closed() {
    let browsing = Browsing::default();
    let mut client = serving(browsing.runner(), settings());
    let run = client.call("run", Some(run_params("run-1", json!({"until": true}))));
    let asked = client.asked("run/until").await;
    client.refuse(&asked, -32000, "orders page never loaded");
    let reply = client.reply(run).await;
    let closed_by_the_reply = browsing.closed.load(Ordering::SeqCst);
    // The run is over, so the server takes the next one.
    let after = client.request("run", Some(run_params("run-2", json!({})))).await;
    assert!(reply.get("error").is_none());
    assert_eq!(reply["result"], errored("run/until: orders page never loaded"));
    assert!(closed_by_the_reply);
    assert_eq!(after["result"]["status"], "complete");
}

#[tokio::test]
async fn test_a_reply_to_the_check_that_is_not_a_boolean_ends_the_run_as_an_error_result() {
    for answer in [Value::Null, json!("yes"), json!(1), json!({"ok": true})] {
        let mut client = serving(Browsing::default().runner(), settings());
        let run = client.call("run", Some(run_params("run-1", json!({"until": true}))));
        let asked = client.asked("run/until").await;
        client.respond(&asked, answer);
        let reply = client.reply(run).await;
        assert_eq!(reply["result"]["status"], "error");
        assert!(reply["result"]["error"].as_str().unwrap().starts_with("run/until: the result cannot be used: "));
    }
}

/// Resolves one secret for SHOP, the way the agent does before it types one.
fn typist() -> Arc<dyn Runner> {
    runner(|args: RunArgs| async move {
        (args.on_event.as_ref().unwrap())(browser_event(None, None)).await;
        let secrets: Arc<dyn SecretResolver> = args.secrets.clone().unwrap();
        let value = args.cancel.guard(secrets.resolve("SHOP_PASSWORD", "https://shop.example.com")).await?;
        if args.cancel.is_tripped() {
            return Err(RunError::Cancelled);
        }
        Ok(RunResult { answer: Some(format!("typed {}", value.is_some())), ..result(Status::Complete) })
    })
}

const PASSWORD: &str = r#"[{"name": "SHOP_PASSWORD", "origins": ["https://shop.example.com"]}]"#;
const VALUE: &str = "hunter2-correct-horse";

#[tokio::test]
async fn test_an_error_reply_to_a_secret_ends_the_run_as_an_error_result() {
    let mut client = serving(typist(), settings());
    let secrets: Value = serde_json::from_str(PASSWORD).unwrap();
    let run = client.call("run", Some(run_params("run-1", json!({"secrets": secrets}))));
    let asked = client.asked("secrets/resolve").await;
    client.refuse(&asked, -32000, &format!("the vault refused {VALUE}"));
    let reply = client.reply(run).await;
    // The text is the server's own: a client that is not the SDK could put the value in its message.
    assert_eq!(reply["result"], errored("secrets/resolve: the client could not resolve the secret 'SHOP_PASSWORD'"));
}

#[tokio::test]
async fn test_a_secret_reply_of_the_wrong_shape_ends_the_run_as_an_error_result_that_does_not_repeat_it() {
    let mut client = serving(typist(), settings());
    let secrets: Value = serde_json::from_str(PASSWORD).unwrap();
    let run = client.call("run", Some(run_params("run-1", json!({"secrets": secrets}))));
    let asked = client.asked("secrets/resolve").await;
    client.respond(&asked, json!({"value": VALUE}));
    let reply = client.reply(run).await;
    assert_eq!(reply["result"]["status"], "error");
    assert!(reply["result"]["error"].as_str().unwrap().starts_with("secrets/resolve: the result cannot be used: "));
    assert!(!String::from_utf8_lossy(&client.written).contains(VALUE));
}

#[tokio::test]
async fn test_a_run_that_fails_on_its_own_is_an_error_result() {
    let broken = runner(|_| async { Err(RunError::Failed("ValueError: no such page".into())) });
    let mut client = serving(broken, settings());
    let response = client.request("run", Some(run_params("run-1", json!({"until": true, "frames": true})))).await;
    assert!(response.get("error").is_none());
    assert_eq!(response["result"], errored("ValueError: no such page"));
}

#[tokio::test]
async fn test_an_error_reply_the_server_cannot_read_still_ends_the_run_and_none_of_it_is_repeated() {
    for error in
        [json!("the orders page never loaded"), json!(["no"]), json!({"code": "-32000", "message": 7}), json!({})]
    {
        let mut client = serving(Browsing::default().runner(), settings());
        let run = client.call("run", Some(run_params("run-1", json!({"until": true}))));
        let asked = client.asked("run/until").await;
        client.send(json!({"jsonrpc": "2.0", "id": asked["id"], "error": error}));
        let reply = client.reply(run).await;
        assert_eq!(reply["result"], errored("run/until: the client replied with an error"));
        assert!(!String::from_utf8_lossy(&client.written).contains("never loaded"));
    }
}

#[tokio::test]
async fn test_an_error_reply_with_one_member_of_the_wrong_type_keeps_the_other() {
    let mut client = serving(Browsing::default().runner(), settings());
    let run = client.call("run", Some(run_params("run-1", json!({"until": true}))));
    let asked = client.asked("run/until").await;
    client.send(json!({"jsonrpc": "2.0", "id": asked["id"], "error": {"code": true, "message": "not yet"}}));
    assert_eq!(client.reply(run).await["result"]["error"], "run/until: not yet");
}

#[tokio::test]
async fn test_a_reply_with_both_members_and_a_null_error_is_read_as_its_result() {
    let mut client = serving(Browsing::default().runner(), settings());
    let run = client.call("run", Some(run_params("run-1", json!({"until": true}))));
    let asked = client.asked("run/until").await;
    client.send(json!({"jsonrpc": "2.0", "id": asked["id"], "result": true, "error": null}));
    assert_eq!(client.reply(run).await["result"]["status"], "complete");
}
