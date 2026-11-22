//! A cancelled queued MCP call must not leave an unnamed trace when its slot becomes available.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{Call, result, runner};
use fastbrowse_cli::mcp::{McpServer, Outbox, ServerConfig, Session};
use fastbrowse_cli::runner::RunError;
use fastbrowse_core::models::Status;
use fastbrowse_core::protocol::CancellationReceipt;
use serde_json::{Value, json};
use tokio::sync::{Notify, mpsc};

async fn reply(inbox: &mut mpsc::UnboundedReceiver<Value>, id: i64) -> Value {
    loop {
        let message = tokio::time::timeout(Duration::from_secs(5), inbox.recv()).await.unwrap().unwrap();
        if message["id"] == id {
            return message;
        }
    }
}

#[tokio::test]
async fn test_a_cancelled_queued_call_leaves_no_unnamed_trace() {
    let traces = std::env::temp_dir().join(format!("fastbrowse-queued-cancel-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&traces);
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let (began, allowed) = (started.clone(), release.clone());
    let run = runner(move |call: Call| {
        let (began, allowed) = (began.clone(), allowed.clone());
        async move {
            if call.task.task == "first" {
                began.notify_one();
                allowed.notified().await;
                Ok(result(Status::Complete))
            } else {
                // A real run observes the already-tripped cancellation once it starts.
                assert!(call.host.cancel.is_tripped());
                call.host.http.unwrap().trace.event("cancelled", serde_json::Map::new);
                Err(RunError::Cancelled(CancellationReceipt::default()))
            }
        }
    });
    let config = ServerConfig { traces: Some(traces.clone()), max_concurrent: 1, ..ServerConfig::default() };
    let server = McpServer::new(config, run);
    let session = Arc::new(Session::default());
    let (sender, mut inbox) = mpsc::unbounded_channel();
    let outbox: Outbox = Arc::new(move |message| {
        sender.send(message).unwrap();
    });
    let send = |message: Value| server.receive(&session, message.to_string().as_bytes(), &outbox);
    send(json!({"jsonrpc":"2.0", "id":0, "method":"initialize", "params":{
        "protocolVersion":"2025-06-18", "clientInfo":{"name":"test", "version":"1"}}}));
    reply(&mut inbox, 0).await;
    send(json!({"jsonrpc":"2.0", "id":1, "method":"tools/call", "params":{
        "name":"browse", "arguments":{"task":"first"}}}));
    started.notified().await;
    send(json!({"jsonrpc":"2.0", "id":2, "method":"tools/call", "params":{
        "name":"browse", "arguments":{"task":"second"}}}));
    let waiting = tokio::time::timeout(Duration::from_secs(5), inbox.recv()).await.unwrap().unwrap();
    assert!(waiting["params"]["data"].as_str().unwrap().contains("waiting for another browse call"));
    send(json!({"jsonrpc":"2.0", "method":"notifications/cancelled", "params":{"requestId":2}}));
    let cancelled = reply(&mut inbox, 2).await;
    assert_eq!(cancelled["error"]["message"], "Request cancelled");
    release.notify_one();
    reply(&mut inbox, 1).await;
    // Give the cancelled call's grace-period cleanup a turn after the slot is released.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let retained: Vec<_> =
        std::fs::read_dir(&traces).into_iter().flatten().map(|entry| entry.unwrap().path()).collect();
    let _ = std::fs::remove_dir_all(&traces);
    for path in retained {
        assert!(
            cancelled["error"]["data"].as_str().is_some_and(|data| data.contains(path.to_str().unwrap())),
            "the cancelled queued call retained {} but its reply named no path: {cancelled}",
            path.display()
        );
    }
}
