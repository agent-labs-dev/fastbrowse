//! tests/test_serve.py: `fastbrowse serve --stdio` as a client sees it, the messages on the wire and the process
//! exiting. Most cases drive the server over in-memory streams; the ones about the real pipe start the binary.

mod common;

use std::io::{Read, Write};
use std::process::{Command, Stdio};

use common::{Recorder, binary, serving, settings};
use fastbrowse_core::protocol::{ErrorCode, PROTOCOL_VERSION};
use serde_json::{Value, json};

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn server() -> common::FakeClient {
    serving(Recorder::default().runner(), settings())
}

#[tokio::test]
async fn test_initialize_answers_with_the_protocol_version_and_the_package_version() {
    let mut client = server();
    let response = client.request("initialize", Some(json!({"protocol_version": 1}))).await;
    assert_eq!(
        response,
        json!({"jsonrpc": "2.0", "id": 1, "result": {"protocol_version": 1, "fastbrowse_version": VERSION}})
    );
    assert_eq!(PROTOCOL_VERSION, 1);
}

#[tokio::test]
async fn test_a_client_on_another_protocol_version_still_learns_the_servers() {
    let mut client = server();
    let result = client.result("initialize", Some(json!({"protocol_version": 2, "added_in_two": true}))).await;
    assert_eq!(result["protocol_version"], 1);
}

#[tokio::test]
async fn test_shutdown_is_answered_and_then_the_server_exits_zero() {
    let mut client = server();
    client.result("initialize", Some(json!({"protocol_version": 1}))).await;
    let response = client.request("shutdown", None).await;
    assert_eq!(response, json!({"jsonrpc": "2.0", "id": 2, "result": null}));
    assert_eq!(client.exit_code().await, 0);
}

#[tokio::test]
async fn test_end_of_input_is_a_shutdown() {
    let mut client = server();
    client.result("initialize", Some(json!({"protocol_version": 1}))).await;
    client.close_input();
    assert_eq!(client.exit_code().await, 0);
    assert!(client.silent().await);
}

#[tokio::test]
async fn test_an_unknown_method_is_refused_as_method_not_found() {
    let mut client = server();
    let response = client.request("run/teleport", None).await;
    assert_eq!(response["id"], 1);
    assert_eq!(response["error"]["code"], -32601);
    assert!(response["error"]["message"].as_str().unwrap().contains("run/teleport"));
    assert!(response.get("result").is_none());
}

#[tokio::test]
async fn test_malformed_params_are_refused_as_invalid_params() {
    for (params, named) in [
        (json!({}), "protocol_version"),
        (json!({"protocol_version": "one"}), "protocol_version"),
        (json!([1]), "params"),
    ] {
        let mut client = server();
        client.send(json!({"jsonrpc": "2.0", "id": 7, "method": "initialize", "params": params}));
        let response = client.reply(7).await;
        assert_eq!(response["error"]["code"], -32602);
        assert!(response["error"]["message"].as_str().unwrap().contains(named), "{response}");
    }
}

#[tokio::test]
async fn test_a_param_the_method_does_not_take_is_refused_and_not_dropped() {
    let mut client = server();
    let response = client.request("shutdown", Some(json!({"force": true}))).await;
    assert_eq!(response["error"]["code"], -32602);
    assert!(response["error"]["message"].as_str().unwrap().contains("force"));
    // A refused shutdown did not shut anything down.
    assert_eq!(client.request("initialize", Some(json!({"protocol_version": 1}))).await["id"], 2);
}

#[tokio::test]
async fn test_a_line_that_is_not_json_gets_a_parse_error_and_the_server_keeps_serving() {
    let mut client = server();
    client.send_line(b"{\"jsonrpc\": \"2.0\", \"id\": 1, \"method\"\n");
    let refused = client.receive().await;
    client.send_line(b"\xff\xfe\n");
    let undecodable = client.receive().await;
    let result = client.result("initialize", Some(json!({"protocol_version": 1}))).await;
    let message = "EOF while parsing an object at line 2 column 0";
    assert_eq!(refused, json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": message}}));
    assert_eq!(undecodable["error"]["code"], -32700);
    assert_eq!(result["protocol_version"], 1);
}

#[tokio::test]
async fn test_json_that_is_not_a_request_is_refused_as_invalid_request() {
    for (message, echoed) in [
        (json!([{"jsonrpc": "2.0", "id": 1, "method": "initialize"}]), Value::Null),
        (json!({"jsonrpc": "1.0", "id": 4, "method": "initialize"}), json!(4)),
        (json!({"jsonrpc": "2.0", "id": "abc", "method": 5}), json!("abc")),
        (json!({"jsonrpc": "2.0", "id": true, "method": "initialize"}), Value::Null),
    ] {
        let mut client = server();
        client.send(message);
        let response = client.receive().await;
        assert_eq!(response["error"]["code"], -32600);
        assert_eq!(response["id"], echoed);
    }
}

#[tokio::test]
async fn test_a_notification_is_never_answered_even_when_its_method_is_unknown() {
    let mut client = server();
    client.send(json!({"jsonrpc": "2.0", "method": "run/teleport"}));
    let response = client.request("initialize", Some(json!({"protocol_version": 1}))).await;
    // Had the notification been answered, that answer would have come first and be waiting here.
    assert_eq!(response["id"], 1);
    assert!(client.inbox.is_empty());
}

#[test]
fn test_the_error_codes_are_a_closed_set() {
    let codes: Vec<i64> = ErrorCode::ALL.iter().map(|code| code.code()).collect();
    assert_eq!(codes, [-32700, -32600, -32601, -32602, -32603, -32001, -32002, -32003, -32004]);
    assert!(ErrorCode::from_code(-32099).is_none());
}

const INITIALIZE: &[u8] =
    b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocol_version\":1}}\n";

fn start(arguments: &[&str]) -> std::process::Child {
    Command::new(binary())
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the binary starts")
}

/// Every line the process wrote to stdout, each of which has to be a message.
fn lines(stdout: &[u8]) -> Vec<Value> {
    assert!(stdout.ends_with(b"\n") && !stdout.contains(&b'\r'));
    stdout
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect()
}

#[test]
fn test_the_command_answers_shutdown_and_exits_zero_while_its_input_is_still_open() {
    let mut process = start(&["serve", "--stdio"]);
    let mut stdin = process.stdin.take().unwrap();
    stdin.write_all(INITIALIZE).unwrap();
    stdin.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"shutdown\"}\n").unwrap();
    stdin.flush().unwrap();
    assert!(process.wait().unwrap().success());
    let mut stdout = Vec::new();
    process.stdout.take().unwrap().read_to_end(&mut stdout).unwrap();
    let replies = lines(&stdout);
    assert_eq!(replies[0]["result"], json!({"protocol_version": 1, "fastbrowse_version": VERSION}));
    assert_eq!(replies[1], json!({"jsonrpc": "2.0", "id": 2, "result": null}));
    drop(stdin);
}

#[test]
fn test_the_command_exits_zero_when_its_input_ends() {
    let mut process = start(&["serve", "--stdio"]);
    process.stdin.take().unwrap().write_all(INITIALIZE).unwrap();
    let output = process.wait_with_output().unwrap();
    assert!(output.status.success());
    assert_eq!(lines(&output.stdout).iter().map(|reply| reply["id"].clone()).collect::<Vec<_>>(), [json!(1)]);
}

#[test]
fn test_a_request_line_over_64_kib_is_read_whole() {
    // The id comes back in the reply, so a line cut short or split in two could not produce this one.
    let long_id = "x".repeat(200_000);
    let request = json!({"jsonrpc": "2.0", "id": long_id, "method": "initialize", "params": {"protocol_version": 1}});
    let mut process = start(&["serve", "--stdio"]);
    process.stdin.take().unwrap().write_all(format!("{request}\n").as_bytes()).unwrap();
    let output = process.wait_with_output().unwrap();
    let replies = lines(&output.stdout);
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0]["id"], json!(long_id));
    assert_eq!(replies[0]["result"]["protocol_version"], 1);
}

#[tokio::test]
async fn test_reading_a_long_line_takes_time_in_proportion_to_its_length() {
    // An attachment makes a `run` one line of tens of megabytes. Joining what has arrived to each new chunk copies
    // the line once per chunk.
    let mut input = vec![b'x'; 96 << 20];
    input.extend_from_slice(b"\nshort\ntail");
    let transport = fastbrowse_cli::serve::StdioTransport::reading(std::io::Cursor::new(input));
    use fastbrowse_cli::serve::Transport;
    let started = std::time::Instant::now();
    let first = transport.receive().await.unwrap();
    let elapsed = started.elapsed();
    let mut rest = Vec::new();
    for _ in 0..3 {
        rest.push(transport.receive().await);
    }
    assert_eq!(first.len(), 96 << 20);
    assert_eq!(rest, [Some(b"short".to_vec()), Some(b"tail".to_vec()), None]);
    assert!(elapsed.as_secs_f64() < 5.0);
}

#[test]
fn test_the_command_logs_to_stderr() {
    let mut process = start(&["serve", "--stdio"]);
    process.stdin.take().unwrap().write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"run/teleport\"}\n").unwrap();
    let output = process.wait_with_output().unwrap();
    let replies = lines(&output.stdout);
    assert_eq!(replies[0]["error"]["code"], -32601);
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("fastbrowse: WARNING serve: unknown method 'run/teleport'")
    );
}

#[test]
fn test_serve_without_a_transport_is_a_usage_error() {
    let output = start(&["serve"]).wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--stdio"));
}

#[test]
fn test_help_matches_pythons() {
    let output = start(&["serve", "--help"]).wait_with_output().unwrap();
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout), include_str!("../generated/serve.txt"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("run-task"));
}
