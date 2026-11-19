//! A desktop run against a fake `deskpilot`: a shell script that answers MCP from reply files and logs what it was
//! sent, so the requests fastbrowse makes and the environment it starts deskpilot with can be read back.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use fastbrowse_browser::pointer::PointerMark;
use fastbrowse_core::models::{Operation, StepOutcome};
use fastbrowse_core::page::Action;
use fastbrowse_core::page::Page;
use fastbrowse_run::desktop::{DeskpilotPage, Film, OpenError, View};
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
    let page = DeskpilotPage::open(program.to_str().unwrap(), &command(), View::Hidden).await.unwrap();
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
    let error = DeskpilotPage::open(program.to_str().unwrap(), &command(), View::Hidden).await.err().unwrap();
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
    let error = DeskpilotPage::open(program.to_str().unwrap(), &command(), View::Hidden).await.err().unwrap();
    let OpenError::Invalid(message) = error else { panic!("{error:?}") };
    assert!(message.contains("exited before it opened a window"), "{message}");
}

#[tokio::test]
async fn a_desktop_app_on_xwayland_is_invalid() {
    let program = fake("xwayland", &[launch_tool()], &[ok(3, &window(true))]);
    let error = DeskpilotPage::open(program.to_str().unwrap(), &command(), View::Hidden).await.err().unwrap();
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
    let page = DeskpilotPage::open(program.to_str().unwrap(), &command(), View::Hidden).await.unwrap();
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
    let page = DeskpilotPage::open(program.to_str().unwrap(), &command(), View::Hidden).await.unwrap();
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
    let page = DeskpilotPage::open(program.to_str().unwrap(), &command(), View::Hidden).await.unwrap();
    let before = page.observe().await.unwrap();
    let wait = Action::new(Operation::Wait);
    let result = tokio::time::timeout(std::time::Duration::from_millis(1500), page.wait(&wait, &before, 4.0))
        .await
        .expect("settling a changed window must not consume the remaining run")
        .unwrap();
    assert!(result.page_changed, "{result:?}");
}

/// A `shot` reply naming a PNG the fake wrote beside itself, holding `bytes`.
fn shot(id: u64, dir: &Path, name: &str, bytes: &[u8]) -> Value {
    let path = dir.join(format!("{name}.png"));
    std::fs::write(&path, bytes).unwrap();
    ok(id, &json!({"shot": name, "path": path, "width": 640, "height": 480}))
}

/// A filmed page: the launch, its first frame, one observation, then a wait that changes nothing and the frame after
/// it.
fn filmed(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("desktop").join(format!("{name}-shots"));
    std::fs::create_dir_all(&dir).unwrap();
    let root = json!({"id":"e1", "role":"frame", "snapshot":"s1", "states":["showing"]});
    let replies = [
        ok(3, &window(false)),
        shot(4, &dir, "launched", b"launched"),
        ok(5, &window(false)),
        ok(6, &root),
        ok(7, &window(false)),
        ok(8, &root),
        shot(9, &dir, "waited", b"waited"),
    ];
    let shot_tool = json!({"name": "shot", "inputSchema": {"type": "object", "properties": {"hidden": {}}}});
    fake(name, &[launch_tool(), shot_tool], &replies)
}

/// The frames and cursor points a filmed page sent.
#[derive(Default)]
struct Kept {
    frames: Mutex<Vec<Vec<u8>>>,
    points: Mutex<Vec<PointerMark>>,
}

impl Film for Kept {
    fn frame(&self, png: Vec<u8>) {
        self.frames.lock().unwrap().push(png);
    }

    fn point(&self, mark: PointerMark) {
        self.points.lock().unwrap().push(mark);
    }
}

fn frames() -> (Arc<Kept>, Arc<Kept>) {
    let kept = Arc::new(Kept::default());
    (kept.clone(), kept)
}

#[tokio::test]
async fn a_filmed_desktop_run_frames_its_window_on_launch_and_after_each_action() {
    let program = filmed("filmed");
    let (film, frames) = frames();
    let page =
        DeskpilotPage::open(program.to_str().unwrap(), &command(), View::Hidden).await.unwrap().filmed(film).await;
    let before = page.observe().await.unwrap();
    page.wait(&Action::new(Operation::Wait), &before, 0.0).await.unwrap();
    assert_eq!(*frames.frames.lock().unwrap(), [b"launched".to_vec(), b"waited".to_vec()]);
    let shots: Vec<Value> = requests(&program)
        .into_iter()
        .filter(|request| request.pointer("/params/name") == Some(&json!("shot")))
        .collect();
    // Shot in the run's own view: a hidden window is framed where it is, never brought on screen.
    assert!(shots.iter().all(|shot| shot.pointer("/params/arguments/hidden") == Some(&json!(true))), "{shots:?}");
}

#[tokio::test]
async fn a_filmed_desktop_run_takes_no_frame_while_frames_are_withheld() {
    let program = filmed("withheld");
    let (film, frames) = frames();
    let page =
        DeskpilotPage::open(program.to_str().unwrap(), &command(), View::Hidden).await.unwrap().filmed(film).await;
    let before = page.observe().await.unwrap();
    // As the agent does before typing a secret: the window may show it next, and a frame cannot be masked.
    page.withhold_frames(true);
    page.wait(&Action::new(Operation::Wait), &before, 0.0).await.unwrap();
    assert_eq!(*frames.frames.lock().unwrap(), [b"launched".to_vec()]);
    let shots = requests(&program).iter().filter(|r| r.pointer("/params/name") == Some(&json!("shot"))).count();
    assert_eq!(shots, 1);
}

#[tokio::test]
async fn a_filmed_click_marks_its_element_centre_for_the_recording_cursor() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("desktop").join("pointed-shots");
    std::fs::create_dir_all(&dir).unwrap();
    let button = json!({"id":"e2", "role":"push button", "name":"Save", "states":["showing", "enabled"],
        "x": 100, "y": 40, "width": 80, "height": 20});
    let root = json!({"id":"e1", "role":"frame", "snapshot":"s1", "states":["showing"], "width": 800, "height": 600,
        "children": [button]});
    // The button's own label changes, which the observation shows.
    let mut saved = root.clone();
    saved["children"][0]["name"] = json!("Saved");
    let replies = [
        ok(3, &window(false)),
        shot(4, &dir, "launched", b"launched"),
        ok(5, &window(false)),
        ok(6, &root),
        ok(7, &json!({"x": 140, "y": 50})),
        ok(8, &window(false)),
        ok(9, &saved),
        ok(10, &window(false)),
        ok(11, &saved),
        ok(12, &window(false)),
        ok(13, &saved),
        shot(14, &dir, "clicked", b"clicked"),
    ];
    let shot_tool = json!({"name": "shot", "inputSchema": {"type": "object", "properties": {"hidden": {}}}});
    let program = fake("pointed", &[launch_tool(), shot_tool], &replies);
    let (film, kept) = frames();
    let page =
        DeskpilotPage::open(program.to_str().unwrap(), &command(), View::Hidden).await.unwrap().filmed(film).await;
    let before = page.observe().await.unwrap();
    let mut click = Action::new(Operation::Click);
    click.target_id = Some("e2".into());
    let result = page.act(&click, &before).await.unwrap();
    assert!(result.page_changed, "{result:?}");
    let points = kept.points.lock().unwrap();
    let points: Vec<_> = points.iter().map(|m| (m.operation, m.x, m.y, m.width, m.height)).collect();
    assert_eq!(points, [(Operation::Click, 140.0, 50.0, 800.0, 600.0)]);
    assert_eq!(*kept.frames.lock().unwrap(), [b"launched".to_vec(), b"clicked".to_vec()]);
}
