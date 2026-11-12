//! A desktop run against a fake `deskpilot`: a shell script that answers MCP from reply files and logs what it was
//! sent, so the requests fastbrowse makes and the environment it starts deskpilot with can be read back.

use std::path::{Path, PathBuf};

use fastbrowse_core::models::{Operation, StepOutcome};
use fastbrowse_core::page::Action;
use fastbrowse_core::page::Page;
use fastbrowse_run::desktop::{DeskpilotPage, OpenError};
use serde_json::{Value, json};

const SCRIPT: &str = r#"#!/bin/sh
dir=$(dirname "$0")
printf '%s\n' "$DESKPILOT_AGENT" > "$dir/agent"
n=0
while IFS= read -r line; do
  n=$((n + 1))
  printf '%s\n' "$line" >> "$dir/requests"
  if [ -f "$dir/reply$n" ]; then cat "$dir/reply$n"; fi
done
"#;

fn launch_tool() -> Value {
    json!({"name": "launch", "inputSchema": {"type": "object", "properties": {"command": {"type": "array"}}}})
}

fn window(xwayland: bool) -> Value {
    json!({"address": "0xabc", "class": "org.gnome.TextEditor", "title": "New Document", "xwayland": xwayland})
}

fn ok(id: u64, value: &Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": {"content": [{"type": "text", "text": value.to_string()}]}})
}

/// A fake deskpilot in its own directory: the handshake, a `tools/list` offering `tools`, then `replies` in order for
/// the requests after it (ids 3 on, as line 2 is the `initialized` notification).
fn fake(name: &str, tools: &[Value], replies: &[Value]) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("desktop").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let line =
        |n: usize, message: &Value| std::fs::write(dir.join(format!("reply{n}")), format!("{message}\n")).unwrap();
    line(1, &json!({"jsonrpc": "2.0", "id": 1, "result": {"protocolVersion": "2025-06-18", "capabilities": {}}}));
    line(3, &json!({"jsonrpc": "2.0", "id": 2, "result": {"tools": tools}}));
    for (offset, reply) in replies.iter().enumerate() {
        line(4 + offset, reply);
    }
    let program = dir.join("deskpilot");
    std::fs::write(&program, SCRIPT).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    program
}

fn requests(program: &Path) -> Vec<Value> {
    let log = std::fs::read_to_string(program.with_file_name("requests")).unwrap();
    log.lines().map(|line| serde_json::from_str(line).unwrap()).collect()
}

fn command() -> Vec<String> {
    vec!["gnome-text-editor".into(), "--new-window".into()]
}

#[tokio::test]
async fn a_desktop_run_launches_its_app_as_a_named_deskpilot_agent() {
    let program = fake("launches", &[launch_tool()], &[ok(3, &window(false)), ok(4, &window(false))]);
    let page = DeskpilotPage::open(program.to_str().unwrap(), &command()).await.unwrap();
    page.address().await.unwrap();
    let sent = requests(&program);
    let launch = sent.iter().find(|request| request.pointer("/params/name") == Some(&json!("launch"))).unwrap();
    assert_eq!(launch.pointer("/params/arguments/command"), Some(&json!(["gnome-text-editor", "--new-window"])));
    // The page drives the window `launch` returned.
    assert_eq!(sent.last().unwrap().pointer("/params/arguments/window"), Some(&json!("0xabc")));
    // deskpilot's name rule: up to 32 letters, digits, `-`, `_` or `.`.
    let agent = std::fs::read_to_string(program.with_file_name("agent")).unwrap();
    let agent = agent.trim_end();
    assert!(!agent.is_empty() && agent.len() <= 32, "{agent:?}");
    assert!(agent.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)), "{agent:?}");
}

#[tokio::test]
async fn a_desktop_run_on_a_deskpilot_without_launch_is_unavailable() {
    let program = fake("no-launch", &[], &[]);
    let error = DeskpilotPage::open(program.to_str().unwrap(), &command()).await.err().unwrap();
    assert!(matches!(error, OpenError::Unavailable(_)), "{error:?}");
}

#[tokio::test]
async fn a_desktop_app_that_exits_before_a_window_is_invalid() {
    let refused = json!({"jsonrpc": "2.0", "id": 3, "result": {
        "isError": true,
        "structuredContent": {"code": "refused", "message": "gnome-text-editor exited before it opened a window"},
        "content": [{"type": "text", "text": "gnome-text-editor exited before it opened a window"}],
    }});
    let program = fake("refused", &[launch_tool()], &[refused]);
    let error = DeskpilotPage::open(program.to_str().unwrap(), &command()).await.err().unwrap();
    let OpenError::Invalid(message) = error else { panic!("{error:?}") };
    assert!(message.contains("exited before it opened a window"), "{message}");
}

#[tokio::test]
async fn a_desktop_app_on_xwayland_is_invalid() {
    let program = fake("xwayland", &[launch_tool()], &[ok(3, &window(true))]);
    let error = DeskpilotPage::open(program.to_str().unwrap(), &command()).await.err().unwrap();
    assert!(matches!(error, OpenError::Invalid(_)), "{error:?}");
}

fn waiting_window(name: &str, after: u64, continuous: bool) -> PathBuf {
    let root = json!({"id":"e1", "role":"frame", "snapshot":"s1", "states":["showing"]});
    let mut replies = vec![ok(3, &window(false)), ok(4, &window(false)), ok(5, &root)];
    for read in 0..70 {
        let mut current = window(false);
        if read >= after {
            current["title"] = json!("Ready");
        }
        let mut root = root.clone();
        if continuous && read >= after {
            root["name"] = json!(format!("Progress {read}"));
        }
        replies.push(ok(6 + read * 2, &current));
        replies.push(ok(7 + read * 2, &root));
    }
    fake(name, &[launch_tool()], &replies)
}

#[tokio::test]
async fn a_desktop_wait_uses_the_supplied_remaining_time() {
    let program = waiting_window("wait-budget", 60, false);
    let page = DeskpilotPage::open(program.to_str().unwrap(), &command()).await.unwrap();
    let before = page.observe().await.unwrap();
    let wait = Action::new(Operation::Wait);
    let result = tokio::time::timeout(std::time::Duration::from_secs(1), page.wait(&wait, &before, 0.15))
        .await
        .expect("the supplied deadline must end the wait")
        .unwrap();
    assert_eq!(result.outcome, StepOutcome::Executed);
    assert!(!result.page_changed);
    assert_eq!(result.detail.as_deref(), Some("the window did not change"));
}

#[tokio::test]
async fn an_unlimited_desktop_wait_sees_a_change_after_five_seconds() {
    // Sixty unchanged polls take six seconds, beyond the old five-second WAIT bound.
    let program = waiting_window("wait-unlimited", 60, false);
    let page = DeskpilotPage::open(program.to_str().unwrap(), &command()).await.unwrap();
    let before = page.observe().await.unwrap();
    let wait = Action::new(Operation::Wait);
    let result = tokio::time::timeout(std::time::Duration::from_secs(15), page.wait(&wait, &before, f64::INFINITY))
        .await
        .expect("an unlimited wait must end on the later change")
        .unwrap();
    assert_eq!(result.outcome, StepOutcome::Executed);
    assert!(result.page_changed, "{result:?}");
}

#[tokio::test]
async fn a_desktop_wait_ends_after_a_change_even_when_the_window_keeps_updating() {
    let program = waiting_window("wait-busy", 2, true);
    let page = DeskpilotPage::open(program.to_str().unwrap(), &command()).await.unwrap();
    let before = page.observe().await.unwrap();
    let wait = Action::new(Operation::Wait);
    let result = tokio::time::timeout(std::time::Duration::from_millis(1500), page.wait(&wait, &before, 4.0))
        .await
        .expect("settling a changed window must not consume the remaining run")
        .unwrap();
    assert!(result.page_changed, "{result:?}");
}
