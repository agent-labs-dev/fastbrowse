//! A client for `deskpilot mcp`: one child process for the run, spoken to in MCP's JSON-RPC over its stdin and stdout.

use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;

/// How long one tool call may take. deskpilot bounds its own waits (a hover rests at most 5 seconds, a paste waits
/// for the app to read it), so a call still unanswered after this has lost its server.
const CALL_SECONDS: u64 = 60;

/// Why a tool call failed.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolError {
    /// deskpilot refused or could not do it, with its error code (`stale`, `covered`, `not_found`, ...) and its words.
    Refused { code: String, message: String },
    /// The server stopped answering, or answered with something that is not MCP.
    Transport(String),
}

impl ToolError {
    pub fn message(&self) -> &str {
        match self {
            ToolError::Refused { message, .. } | ToolError::Transport(message) => message,
        }
    }
}

struct Pipe {
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

pub struct Deskpilot {
    // Killed with the client: a run that ends or is cancelled leaves no server behind.
    _child: Child,
    pipe: Mutex<Pipe>,
}

impl Deskpilot {
    /// Starts `program mcp` and completes MCP's handshake with it.
    pub async fn start(program: &str) -> Result<Deskpilot, ToolError> {
        let mut child = Command::new(program)
            .arg("mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| ToolError::Transport(format!("could not start {program} mcp: {error}")))?;
        let pipe = Pipe {
            stdin: child.stdin.take().expect("stdin is piped"),
            stdout: BufReader::new(child.stdout.take().expect("stdout is piped")),
            next_id: 0,
        };
        let client = Deskpilot { _child: child, pipe: Mutex::new(pipe) };
        let version = env!("CARGO_PKG_VERSION");
        client
            .request(
                "initialize",
                json!({
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": {"name": "fastbrowse", "version": version},
                }),
            )
            .await?;
        client.notify("notifications/initialized").await?;
        Ok(client)
    }

    async fn notify(&self, method: &str) -> Result<(), ToolError> {
        let mut pipe = self.pipe.lock().await;
        write_line(&mut pipe.stdin, &json!({"jsonrpc": "2.0", "method": method})).await
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, ToolError> {
        let mut pipe = self.pipe.lock().await;
        pipe.next_id += 1;
        let id = pipe.next_id;
        write_line(&mut pipe.stdin, &json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})).await?;
        let read = async {
            let mut line = String::new();
            loop {
                line.clear();
                let read = pipe.stdout.read_line(&mut line).await;
                match read {
                    Ok(0) => return Err(ToolError::Transport("deskpilot mcp exited".into())),
                    Err(error) => return Err(ToolError::Transport(format!("reading deskpilot mcp: {error}"))),
                    Ok(_) => {}
                }
                let message: Value = serde_json::from_str(&line).map_err(|error| {
                    ToolError::Transport(format!("deskpilot mcp sent a line that is not JSON: {error}"))
                })?;
                // Notifications and log messages carry no id; only this request's reply ends the wait.
                if message.get("id").and_then(Value::as_u64) != Some(id) {
                    continue;
                }
                if let Some(error) = message.get("error") {
                    return Err(ToolError::Transport(format!("deskpilot mcp refused {method}: {error}")));
                }
                return Ok(message.get("result").cloned().unwrap_or(Value::Null));
            }
        };
        match tokio::time::timeout(Duration::from_secs(CALL_SECONDS), read).await {
            Ok(result) => result,
            Err(_) => Err(ToolError::Transport(format!("deskpilot mcp did not answer {method} in {CALL_SECONDS} s"))),
        }
    }

    /// Calls a tool, returning its content blocks, or its refusal with the code deskpilot gave it.
    pub async fn call(&self, tool: &str, arguments: Value) -> Result<Vec<Value>, ToolError> {
        let result = self.request("tools/call", json!({"name": tool, "arguments": arguments})).await?;
        let content = result.get("content").and_then(Value::as_array).cloned().unwrap_or_default();
        if result.get("isError").and_then(Value::as_bool) == Some(true) {
            let structured = result.get("structuredContent");
            let field = |name: &str| structured.and_then(|s| s.get(name)).and_then(Value::as_str).map(str::to_string);
            let message = field("message")
                .or_else(|| content.first().and_then(|c| c.get("text")).and_then(Value::as_str).map(str::to_string))
                .unwrap_or_else(|| format!("deskpilot {tool} failed"));
            // A failure deskpilot did not expect has no code: it is not a refusal a caller can act on.
            return Err(match field("code") {
                Some(code) => ToolError::Refused { code, message },
                None => ToolError::Transport(message),
            });
        }
        Ok(content)
    }

    /// Calls a tool whose result is one JSON value, which MCP carries as the text of its first content block.
    pub async fn call_json(&self, tool: &str, arguments: Value) -> Result<Value, ToolError> {
        let content = self.call(tool, arguments).await?;
        json_block(&content).ok_or_else(|| ToolError::Transport(format!("deskpilot {tool} returned no JSON")))
    }
}

/// The first text block of a tool result that parses as JSON.
pub fn json_block(content: &[Value]) -> Option<Value> {
    content
        .iter()
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .find_map(|text| serde_json::from_str(text).ok())
}

async fn write_line(stdin: &mut ChildStdin, message: &Value) -> Result<(), ToolError> {
    let mut line = message.to_string();
    line.push('\n');
    stdin
        .write_all(line.as_bytes())
        .await
        .map_err(|error| ToolError::Transport(format!("writing to deskpilot mcp: {error}")))?;
    stdin.flush().await.map_err(|error| ToolError::Transport(format!("writing to deskpilot mcp: {error}")))
}
