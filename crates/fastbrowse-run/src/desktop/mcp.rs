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
/// How long a closing server has to end once its apps are stopped and its stdin is closed, before it is killed.
const CLOSE_SECONDS: u64 = 10;

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
    /// None once the client is closed: the server reads end of input and exits.
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    /// The bytes of a line read so far. A call abandoned partway through a line, by its timeout or by the run being
    /// cancelled, left them in a buffer local to the call, and the next call began mid-line and failed as "not JSON"
    /// (#1401); kept here, the next read finishes the line instead.
    partial: Vec<u8>,
    next_id: u64,
}

pub struct Deskpilot {
    // Killed with the client as a last resort: `close` is what lets deskpilot close the apps first, since a kill skips
    // deskpilot's own cleanup and left a run's apps open after it (#1345).
    child: Mutex<Option<Child>>,
    pipe: Mutex<Pipe>,
    /// Where the run's window is acted on, and so which flag every call carries.
    view: View,
    /// The tools whose input schema has the view's flag. A deskpilot before cjber/deskpilot#84 has no `hidden` on
    /// `key`, `type_text` or `paste_text`, and a newer one refuses keys to a window off screen without it, so the
    /// server's own schema says which calls carry it.
    placing: HashSet<String>,
    /// Every tool the server offers, so a page can tell a deskpilot that lacks one it needs.
    tools: HashSet<String>,
}

/// Where a run acts on its window. Hidden keeps it on the agent's hidden workspace, with the user's pointer, focus and
/// view untouched and no agent cursor. Shown brings it into view and acts there, where deskpilot draws the agent's
/// cursor at each action: for a desktop of its own, such as a nested compositor a recording is taken of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum View {
    Hidden,
    Shown,
}

impl View {
    /// The deskpilot argument that places a call in this view.
    fn flag(self) -> &'static str {
        match self {
            View::Hidden => "hidden",
            View::Shown => "show",
        }
    }
}

impl Deskpilot {
    /// Starts `program mcp` as the deskpilot agent named `agent` and completes MCP's handshake with it. The server
    /// owns the apps it launches for that agent, which `close` closes.
    pub async fn start(program: &str, agent: &str, view: View) -> Result<Deskpilot, ToolError> {
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
            stdin: Some(child.stdin.take().expect("stdin is piped")),
            stdout: BufReader::new(child.stdout.take().expect("stdout is piped")),
            partial: Vec::new(),
            next_id: 0,
        };
        let mut client = Deskpilot {
            child: Mutex::new(Some(child)),
            pipe: Mutex::new(pipe),
            view,
            placing: HashSet::new(),
            tools: HashSet::new(),
        };
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
        client.placing = taking(&tools, view.flag());
        client.tools =
            tools.iter().filter_map(|tool| tool.get("name").and_then(Value::as_str).map(str::to_string)).collect();
        Ok(client)
    }

    /// Closes every app this agent launched with deskpilot's `stop`, then ends the server by closing its input, and
    /// kills it only if it has not exited in time. A second call does nothing. The error is `stop`'s, after the
    /// server is ended anyway.
    pub async fn close(&self) -> Result<(), ToolError> {
        let Some(mut child) = self.child.lock().await.take() else { return Ok(()) };
        let stopped = if self.offers("stop") { self.call("stop", json!({})).await.map(|_| ()) } else { Ok(()) };
        self.pipe.lock().await.stdin = None;
        if tokio::time::timeout(Duration::from_secs(CLOSE_SECONDS), child.wait()).await.is_err() {
            let _ = child.kill().await;
        }
        stopped
    }

    /// Whether the server offers `tool`.
    pub fn offers(&self, tool: &str) -> bool {
        self.tools.contains(tool)
    }

    /// `arguments` with the view's flag set when `tool`'s schema offers it.
    pub fn placed(&self, tool: &str, arguments: Value) -> Value {
        with_flag(&self.placing, self.view.flag(), tool, arguments)
    }

    async fn notify(&self, method: &str) -> Result<(), ToolError> {
        let mut pipe = self.pipe.lock().await;
        write_line(pipe.stdin.as_mut(), &json!({"jsonrpc": "2.0", "method": method})).await
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, ToolError> {
        let mut pipe = self.pipe.lock().await;
        pipe.next_id += 1;
        let id = pipe.next_id;
        let request = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        write_line(pipe.stdin.as_mut(), &request).await?;
        let read = async {
            let Pipe { stdout, partial, .. } = &mut *pipe;
            loop {
                // `read_until` is cancel safe: what it read before a cancellation stays in `partial`.
                match stdout.read_until(b'\n', partial).await {
                    Ok(0) => return Err(ToolError::Transport("deskpilot mcp exited".into())),
                    Err(error) => return Err(ToolError::Transport(format!("reading deskpilot mcp: {error}"))),
                    Ok(_) if partial.last() != Some(&b'\n') => {
                        return Err(ToolError::Transport("deskpilot mcp exited partway through a line".into()));
                    }
                    Ok(_) => {}
                }
                let line = std::mem::take(partial);
                // The line itself is the evidence of which side wrote it, so the error quotes it whole.
                let message: Value = serde_json::from_slice(&line).map_err(|error| {
                    ToolError::Transport(format!(
                        "deskpilot mcp sent a line that is not JSON ({error}): {}",
                        String::from_utf8_lossy(&line).trim_end()
                    ))
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
        let result = self.result(tool, arguments).await?;
        Ok(result.get("content").and_then(Value::as_array).cloned().unwrap_or_default())
    }

    /// Calls a tool whose result is a list. MCP carries a list as one text block per item, and as the list itself in
    /// `structuredContent` where the server gives one.
    pub async fn call_list(&self, tool: &str, arguments: Value) -> Result<Vec<Value>, ToolError> {
        let result = self.result(tool, arguments).await?;
        if let Some(items) = result.pointer("/structuredContent/result").and_then(Value::as_array) {
            return Ok(items.clone());
        }
        let content = result.get("content").and_then(Value::as_array).cloned().unwrap_or_default();
        Ok(content
            .iter()
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .filter_map(|text| serde_json::from_str(text).ok())
            .collect())
    }

    /// A tool call's whole result, or its refusal with the code deskpilot gave it.
    async fn result(&self, tool: &str, arguments: Value) -> Result<Value, ToolError> {
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
        Ok(result)
    }

    /// Calls a tool whose result is one JSON value, which MCP carries as the text of its first content block.
    pub async fn call_json(&self, tool: &str, arguments: Value) -> Result<Value, ToolError> {
        let content = self.call(tool, arguments).await?;
        json_block(&content).ok_or_else(|| ToolError::Transport(format!("deskpilot {tool} returned no JSON")))
    }
}

/// The names of the tools in a `tools/list` result whose input schema has the property `flag`.
fn taking(tools: &[Value], flag: &str) -> HashSet<String> {
    tools
        .iter()
        .filter(|tool| tool.pointer(&format!("/inputSchema/properties/{flag}")).is_some())
        .filter_map(|tool| tool.get("name").and_then(Value::as_str).map(str::to_string))
        .collect()
}

fn with_flag(taking: &HashSet<String>, flag: &str, tool: &str, mut arguments: Value) -> Value {
    if taking.contains(tool)
        && let Some(object) = arguments.as_object_mut()
    {
        object.insert(flag.into(), Value::Bool(true));
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

async fn write_line(stdin: Option<&mut ChildStdin>, message: &Value) -> Result<(), ToolError> {
    let stdin = stdin.ok_or_else(|| ToolError::Transport("deskpilot mcp is closed".into()))?;
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
        let (offered, older) = (taking(&with, "hidden"), taking(&without, "hidden"));
        for name in ["key", "type_text", "paste_text"] {
            let sent = with_flag(&offered, "hidden", name, json!({"window": "0x1", "text": "a"}));
            assert_eq!(sent, json!({"window": "0x1", "text": "a", "hidden": true}), "{name}");
            let sent = with_flag(&older, "hidden", name, json!({"window": "0x1", "text": "a"}));
            assert_eq!(sent, json!({"window": "0x1", "text": "a"}), "{name}");
        }
    }

    #[test]
    fn a_shown_run_brings_the_window_into_view_instead_of_hiding_it() {
        let click = [tool("click", json!({"window": {}, "show": {}, "hidden": {}}))];
        let shown = taking(&click, View::Shown.flag());
        let sent = with_flag(&shown, View::Shown.flag(), "click", json!({"window": "0x1"}));
        assert_eq!(sent, json!({"window": "0x1", "show": true}));
    }
}
