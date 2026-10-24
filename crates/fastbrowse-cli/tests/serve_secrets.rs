//! tests/test_serve_secrets.py: secrets over the serve protocol. The refs go to the server, and a value comes back
//! only when it is typed.

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use common::{CHROME, FakeClient, binary, browser_event, result, run_params, runner, serving, settings, step_event};
use fastbrowse_cli::runner::{RunArgs, RunError, Runner};
use fastbrowse_core::models::{RunResult, SecretRef, Status};
use serde_json::{Value, json};

const SHOP: &str = "https://shop.example.com";
const VALUE: &str = "hunter2-correct-horse";

fn password() -> Value {
    json!({"name": "SHOP_PASSWORD", "origins": [SHOP]})
}

/// Stands where `run_task` does, and resolves each `(name, origin)` it was given, in order, sending an event after
/// each and answering with how many resolved.
#[derive(Clone, Default)]
struct Typist {
    typed: Vec<(&'static str, &'static str)>,
    refs: Arc<Mutex<Option<Vec<SecretRef>>>>,
    got_resolver: Arc<Mutex<Option<bool>>>,
    resolved: Arc<Mutex<Vec<Option<String>>>>,
}

impl Typist {
    fn new(typed: &[(&'static str, &'static str)]) -> Typist {
        Typist { typed: typed.to_vec(), ..Typist::default() }
    }

    fn runner(&self) -> Arc<dyn Runner> {
        let typist = self.clone();
        runner(move |args: RunArgs| {
            let typist = typist.clone();
            async move {
                *typist.got_resolver.lock().unwrap() = Some(args.secrets.is_some());
                *typist.refs.lock().unwrap() = args.secrets.as_ref().map(|secrets| secrets.available());
                (args.on_event.as_ref().unwrap())(browser_event(None, None)).await;
                for (index, (name, origin)) in typist.typed.iter().enumerate() {
                    let secrets = args.secrets.clone().unwrap();
                    let value = args.cancel.guard(secrets.resolve(name, origin)).await?;
                    if args.cancel.is_tripped() {
                        return Err(RunError::Cancelled);
                    }
                    typist.resolved.lock().unwrap().push(value);
                    (args.on_event.as_ref().unwrap())(step_event(index as i64 + 1, None)).await;
                }
                let count = typist.resolved.lock().unwrap().iter().filter(|value| value.is_some()).count();
                Ok(RunResult {
                    answer: Some(format!("typed {count} of {}", typist.typed.len())),
                    ..result(Status::Complete)
                })
            }
        })
    }

    fn resolved(&self) -> Vec<Option<String>> {
        self.resolved.lock().unwrap().clone()
    }
}

/// One run as the client saw it: its reply, every `secrets/resolve` the server sent, and every line it wrote.
struct Exchange {
    reply: Value,
    asked: Vec<Value>,
}

/// Send a `run` and answer each `secrets/resolve` with the next of `answers`, until the run's reply.
async fn exchange(client: &mut FakeClient, params: Value, answers: Vec<Value>) -> Exchange {
    let run = client.call("run", Some(params));
    let mut asked = Vec::new();
    let mut answers = answers.into_iter();
    loop {
        let message = client.receive().await;
        if message.get("method") == Some(&json!("secrets/resolve")) {
            client.respond(&message, answers.next().expect("an answer for each question"));
            asked.push(message);
        } else if message.get("method").is_none() && message["id"] == run {
            return Exchange { reply: message, asked };
        }
    }
}

#[tokio::test]
async fn test_declared_refs_reach_run_task_as_a_resolver_that_lists_them() {
    let typist = Typist::new(&[]);
    let mut client = serving(typist.runner(), settings());
    let refs =
        json!([password(), {"name": "SHOP_TOTP", "origins": ["https://*.example.com", "https://login.example.net"]}]);
    let done = exchange(&mut client, run_params("run-1", json!({"secrets": refs})), vec![]).await;
    assert_eq!(done.reply["result"]["status"], "complete");
    assert_eq!(
        typist.refs.lock().unwrap().clone().unwrap(),
        [
            SecretRef { name: "SHOP_PASSWORD".into(), origins: vec![SHOP.into()] },
            SecretRef {
                name: "SHOP_TOTP".into(),
                origins: vec!["https://*.example.com".into(), "https://login.example.net".into()],
            },
        ]
    );
    assert!(done.asked.is_empty());
}

#[tokio::test]
async fn test_a_run_that_declares_no_secrets_gets_no_resolver() {
    let typist = Typist::new(&[]);
    let mut client = serving(typist.runner(), settings());
    let done = exchange(&mut client, run_params("run-1", json!({})), vec![]).await;
    assert_eq!(done.reply["result"]["status"], "complete");
    assert_eq!(*typist.got_resolver.lock().unwrap(), Some(false));
}

#[tokio::test]
async fn test_resolving_on_a_covered_origin_asks_the_client_and_returns_its_value() {
    let typist = Typist::new(&[("SHOP_PASSWORD", SHOP)]);
    let mut client = serving(typist.runner(), settings());
    let done = exchange(&mut client, run_params("run-7", json!({"secrets": [password()]})), vec![json!(VALUE)]).await;
    assert_eq!(
        done.asked,
        [json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "secrets/resolve",
            "params": {"run_id": "run-7", "name": "SHOP_PASSWORD", "origin": SHOP},
        })]
    );
    assert_eq!(typist.resolved(), [Some(VALUE.to_string())]);
}

#[tokio::test]
async fn test_a_wildcard_origin_covers_the_hosts_under_it() {
    let typist = Typist::new(&[("SHOP_PASSWORD", "https://accounts.example.com")]);
    let mut client = serving(typist.runner(), settings());
    let wildcard = json!({"name": "SHOP_PASSWORD", "origins": ["https://*.example.com"]});
    let done = exchange(&mut client, run_params("run-1", json!({"secrets": [wildcard]})), vec![json!(VALUE)]).await;
    let origins: Vec<Value> = done.asked.iter().map(|asked| asked["params"]["origin"].clone()).collect();
    assert_eq!(origins, [json!("https://accounts.example.com")]);
    assert_eq!(typist.resolved(), [Some(VALUE.to_string())]);
}

#[tokio::test]
async fn test_an_origin_the_ref_does_not_cover_resolves_to_nothing_and_the_client_is_not_asked() {
    for origin in
        ["https://evil.test", "http://shop.example.com", "https://shop.example.com:8443", "https://example.com"]
    {
        let typist = Typist::new(&[("SHOP_PASSWORD", origin)]);
        let mut client = serving(typist.runner(), settings());
        let done = exchange(&mut client, run_params("run-1", json!({"secrets": [password()]})), vec![]).await;
        assert_eq!(typist.resolved(), [None]);
        assert!(done.asked.is_empty());
    }
}

#[tokio::test]
async fn test_a_wildcard_does_not_cover_a_host_that_only_ends_with_the_same_letters() {
    let typist =
        Typist::new(&[("SHOP_PASSWORD", "https://example.com.evil.test"), ("SHOP_PASSWORD", "https://notexample.com")]);
    let mut client = serving(typist.runner(), settings());
    let wildcard = json!({"name": "SHOP_PASSWORD", "origins": ["https://*.example.com"]});
    let done = exchange(&mut client, run_params("run-1", json!({"secrets": [wildcard]})), vec![]).await;
    assert_eq!(typist.resolved(), [None, None]);
    assert!(done.asked.is_empty());
}

#[tokio::test]
async fn test_a_name_that_was_never_declared_resolves_to_nothing_and_the_client_is_not_asked() {
    let typist = Typist::new(&[("ADMIN_PASSWORD", SHOP)]);
    let mut client = serving(typist.runner(), settings());
    let done = exchange(&mut client, run_params("run-1", json!({"secrets": [password()]})), vec![]).await;
    assert_eq!(typist.resolved(), [None]);
    assert!(done.asked.is_empty());
}

#[tokio::test]
async fn test_each_resolution_asks_again_so_a_one_time_code_is_fresh() {
    let typist = Typist::new(&[("SHOP_TOTP", SHOP), ("SHOP_TOTP", SHOP)]);
    let mut client = serving(typist.runner(), settings());
    let totp = json!({"name": "SHOP_TOTP", "origins": [SHOP]});
    let params = run_params("run-1", json!({"secrets": [totp]}));
    let done = exchange(&mut client, params, vec![json!("492817"), json!("030551")]).await;
    assert_eq!(done.asked.iter().map(|a| a["id"].clone()).collect::<Vec<_>>(), [json!(1), json!(2)]);
    assert_eq!(typist.resolved(), [Some("492817".to_string()), Some("030551".to_string())]);
}

#[tokio::test]
async fn test_a_null_reply_resolves_to_nothing() {
    let typist = Typist::new(&[("SHOP_PASSWORD", SHOP)]);
    let mut client = serving(typist.runner(), settings());
    let done = exchange(&mut client, run_params("run-1", json!({"secrets": [password()]})), vec![Value::Null]).await;
    assert_eq!(done.asked.len(), 1);
    assert_eq!(typist.resolved(), [None]);
}

#[tokio::test]
async fn test_the_value_is_in_no_message_the_server_wrote() {
    let typist = Typist::new(&[("SHOP_PASSWORD", SHOP), ("SHOP_PASSWORD", SHOP)]);
    let mut client = serving(typist.runner(), settings());
    let params = run_params("run-1", json!({"secrets": [password()]}));
    let done = exchange(&mut client, params, vec![json!(VALUE), json!(VALUE)]).await;
    // An answer nobody is waiting for any more, as after a run that was stopped with its question out.
    client.send(json!({"jsonrpc": "2.0", "id": 99, "result": VALUE}));
    client.request("initialize", Some(json!({"protocol_version": 1}))).await;
    assert_eq!(typist.resolved(), [Some(VALUE.to_string()), Some(VALUE.to_string())]);
    assert_eq!(done.reply["result"]["answer"], "typed 2 of 2");
    let written = String::from_utf8_lossy(&client.written).to_string();
    // The events and the reply sit between the questions and after them, and none of them carries it.
    assert_eq!(written.matches("run/event").count(), 3);
    assert!(client.inbox.is_empty());
    assert!(!written.contains(VALUE));
}

#[tokio::test]
async fn test_an_answer_to_no_request_gets_no_reply() {
    let mut client = serving(Typist::new(&[]).runner(), settings());
    client.send(json!({"jsonrpc": "2.0", "id": 99, "result": VALUE}));
    client.send(json!({"jsonrpc": "2.0", "id": 98, "error": {"code": -32000, "message": "no vault"}}));
    let handshake = client.request("initialize", Some(json!({"protocol_version": 1}))).await;
    assert_eq!(handshake["result"]["protocol_version"], 1);
    assert!(client.inbox.is_empty());
}

#[tokio::test]
async fn test_a_reply_that_is_neither_text_nor_null_raises_without_repeating_it() {
    for answer in [json!({"value": VALUE}), json!([VALUE]), json!(8675309), json!(true)] {
        let typist = Typist::new(&[("SHOP_PASSWORD", SHOP)]);
        let mut client = serving(typist.runner(), settings());
        let done = exchange(&mut client, run_params("run-1", json!({"secrets": [password()]})), vec![answer]).await;
        assert_eq!(
            done.reply["result"]["error"],
            "secrets/resolve: the result cannot be used: Input should be a valid string"
        );
        assert!(typist.resolved().is_empty());
        let written = String::from_utf8_lossy(&client.written).to_string();
        let replied =
            written.split('\n').filter(|line| !line.contains("\"secrets/resolve\",\"params\"")).collect::<String>();
        assert!(!replied.contains(VALUE) && !replied.contains("8675309"));
    }
}

#[tokio::test]
async fn test_the_server_answers_other_requests_while_a_value_is_awaited() {
    let typist = Typist::new(&[("SHOP_PASSWORD", SHOP)]);
    let mut client = serving(typist.runner(), settings());
    let run = client.call("run", Some(run_params("run-1", json!({"secrets": [password()]}))));
    let asked = client.asked("secrets/resolve").await;
    let handshake = client.result("initialize", Some(json!({"protocol_version": 1}))).await;
    client.respond(&asked, json!(VALUE));
    let reply = client.reply(run).await;
    assert_eq!(handshake["protocol_version"], 1);
    assert_eq!(reply["result"]["status"], "complete");
    assert_eq!(typist.resolved(), [Some(VALUE.to_string())]);
}

#[tokio::test]
async fn test_input_ending_while_a_value_is_awaited_stops_the_server() {
    let typist = Typist::new(&[("SHOP_PASSWORD", SHOP)]);
    let mut client = serving(typist.runner(), settings());
    client.call("run", Some(run_params("run-1", json!({"secrets": [password()]}))));
    client.asked("secrets/resolve").await;
    client.close_input();
    assert_eq!(client.exit_code().await, 0);
    assert!(typist.resolved().is_empty());
}

#[tokio::test]
async fn test_secrets_that_are_not_refs_are_refused_without_repeating_them() {
    for (secrets, named) in [
        (json!([{"name": "SHOP_PASSWORD"}]), "secrets.0.origins"),
        (json!([{"name": "SHOP_PASSWORD", "origins": [SHOP], "value": VALUE}]), "secrets.0.value"),
        (json!({"SHOP_PASSWORD": VALUE}), "secrets"),
    ] {
        let typist = Typist::new(&[]);
        let mut client = serving(typist.runner(), settings());
        let response = client.request("run", Some(run_params("run-1", json!({"secrets": secrets})))).await;
        assert_eq!(response["error"]["code"], -32602);
        assert!(response["error"]["message"].as_str().unwrap().contains(named));
        assert!(!response.to_string().contains(VALUE));
        assert!(typist.got_resolver.lock().unwrap().is_none());
    }
}

#[test]
fn test_the_served_command_keeps_the_value_out_of_stdout_and_stderr() {
    let params = run_params(
        "run-1",
        json!({"local": true, "chrome": {"binary": CHROME}, "start": format!("{SHOP}/login"), "secrets": [password()]}),
    );
    let mut process = Command::new(binary())
        .args(["serve", "--stdio", "--run-task", "tests.serve_scripts:type_secret"])
        .env("OPENROUTER_API_KEY", "unused")
        .env("AI_GATEWAY_API_KEY", "unused")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = process.stdin.take().unwrap();
    let mut stdout = BufReader::new(process.stdout.take().unwrap());
    let mut line = json!({"jsonrpc": "2.0", "id": 1, "method": "run", "params": params}).to_string();
    line.push('\n');
    stdin.write_all(line.as_bytes()).unwrap();
    stdin.flush().unwrap();
    let mut asked = String::new();
    stdout.read_line(&mut asked).unwrap();
    let question: Value = serde_json::from_str(&asked).unwrap();
    let mut answer = json!({"jsonrpc": "2.0", "id": question["id"], "result": VALUE}).to_string();
    answer.push('\n');
    stdin.write_all(answer.as_bytes()).unwrap();
    stdin.flush().unwrap();
    let (mut event, mut reply) = (String::new(), String::new());
    stdout.read_line(&mut event).unwrap();
    stdout.read_line(&mut reply).unwrap();
    drop(stdin);
    let mut rest = String::new();
    stdout.read_to_string(&mut rest).unwrap();
    let mut stderr = String::new();
    process.stderr.take().unwrap().read_to_string(&mut stderr).unwrap();
    assert!(process.wait().unwrap().success());
    assert_eq!(question["params"], json!({"run_id": "run-1", "name": "SHOP_PASSWORD", "origin": SHOP}));
    // The stand-in's own proof that the value reached it, in a form that is not the value.
    let reply_json: Value = serde_json::from_str(&reply).unwrap();
    assert_eq!(reply_json["result"]["answer"], format!("typed {} characters", VALUE.len()));
    assert_eq!(serde_json::from_str::<Value>(&event).unwrap()["method"], "run/event");
    for written in [&asked, &event, &reply, &rest, &stderr] {
        assert!(!written.contains(VALUE));
    }
}
