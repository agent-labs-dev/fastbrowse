//! A client for `deskpilot mcp`: one child process for the run, spoken to in MCP's JSON-RPC over its stdin and stdout.

use std::collections::HashSet;
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
    /// The tools whose input schema has `hidden`. A deskpilot before cjber/deskpilot#84 has no `hidden` on `key`,
    /// `type_text` or `paste_text`, and a newer one refuses keys to a window off screen without it, so the server's
    /// own schema says which calls carry it.
    hidden: HashSet<String>,
    /// Every tool the server offers, so a page can tell a deskpilot that lacks one it needs.
    tools: HashSet<String>,
}

impl Deskpilot {
    /// Starts `program mcp` as the deskpilot agent named `agent` and completes MCP's handshake with it. The server
    /// owns the apps it launches for that agent, so they close when it is killed with the client.
    pub async fn start(program: &str, agent: &str) -> Result<Deskpilot, ToolError> {
        let mut child = Command::new(program)
            .arg("mcp")
            .env("DESKPILOT_AGENT", agent)
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
        let mut client =
            Deskpilot { _child: child, pipe: Mutex::new(pipe), hidden: HashSet::new(), tools: HashSet::new() };
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
        let mut tools = Vec::new();
        let mut cursor: Option<Value> = None;
        loop {
            let params = cursor.map_or_else(|| json!({}), |cursor| json!({"cursor": cursor}));
            let page = client.request("tools/list", params).await?;
            tools.extend(page.get("tools").and_then(Value::as_array).cloned().unwrap_or_default());
            cursor = page.get("nextCursor").filter(|next| !next.is_null()).cloned();
            if cursor.is_none() {
                break;
            }
        }
        client.hidden = taking_hidden(&tools);
        client.tools =
            tools.iter().filter_map(|tool| tool.get("name").and_then(Value::as_str).map(str::to_string)).collect();
        Ok(client)
    }

    /// Whether the server offers `tool`.
    pub fn offers(&self, tool: &str) -> bool {
        self.tools.contains(tool)
    }

    /// `arguments` with `"hidden": true` added when `tool`'s schema offers it.
    pub fn hidden(&self, tool: &str, arguments: Value) -> Value {
        with_hidden(&self.hidden, tool, arguments)
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

/// The names of the tools in a `tools/list` result whose input schema has a `hidden` property.
fn taking_hidden(tools: &[Value]) -> HashSet<String> {
    tools
        .iter()
        .filter(|tool| tool.pointer("/inputSchema/properties/hidden").is_some())
        .filter_map(|tool| tool.get("name").and_then(Value::as_str).map(str::to_string))
        .collect()
}

fn with_hidden(taking: &HashSet<String>, tool: &str, mut arguments: Value) -> Value {
    if taking.contains(tool)
        && let Some(object) = arguments.as_object_mut()
    {
        object.insert("hidden".into(), Value::Bool(true));
    }
    arguments
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn tool(name: &str, properties: Value) -> Value {
        json!({"name": name, "inputSchema": {"type": "object", "properties": properties}})
    }

    #[test]
    fn keys_and_text_carry_hidden_only_when_the_schema_offers_it() {
        let with = ["key", "type_text", "paste_text"]
            .map(|name| tool(name, json!({"window": {"type": "string"}, "hidden": {"type": "boolean"}})));
        let without = ["key", "type_text", "paste_text"].map(|name| tool(name, json!({"window": {"type": "string"}})));
        let (offered, older) = (taking_hidden(&with), taking_hidden(&without));
        for name in ["key", "type_text", "paste_text"] {
            let sent = with_hidden(&offered, name, json!({"window": "0x1", "text": "a"}));
            assert_eq!(sent, json!({"window": "0x1", "text": "a", "hidden": true}), "{name}");
            let sent = with_hidden(&older, name, json!({"window": "0x1", "text": "a"}));
            assert_eq!(sent, json!({"window": "0x1", "text": "a"}), "{name}");
        }
    }
}
