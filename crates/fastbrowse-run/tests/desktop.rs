//! A desktop run against a fake `deskpilot`: a shell script that answers MCP from reply files and logs what it was
//! sent, so the requests fastbrowse makes and the environment it starts deskpilot with can be read back.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use fastbrowse_browser::pointer::PointerMark;
use fastbrowse_core::models::{Operation, StepOutcome};
use fastbrowse_core::page::Action;
use fastbrowse_core::page::BrowserError;
use fastbrowse_core::page::Page;
use fastbrowse_run::desktop::{Deskpilot, DeskpilotPage, Film, OpenError, View, connect};
use serde_json::{Value, json};

/// Answers each request with the next reply for its tool, `<tool>.<n>`, repeating the last one once they run out: a
/// page polls the same tool until what it waits for shows. A reply kept for one window, `<tool>@<window>.<n>`, answers
/// that window's requests. Notifications get no reply.
const SCRIPT: &str = r#"#!/bin/sh
dir=$(dirname "$0")
printf '%s\n' "$DESKPILOT_AGENT" > "$dir/agent"
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$dir/requests"
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  [ -z "$id" ] && continue
  case "$line" in
    *'"method":"initialize"'*) key=initialize ;;
    *'"method":"tools/list"'*) key=tools ;;
    *) key=$(printf '%s' "$line" | sed -n 's/.*"name":"\([^"]*\)".*/\1/p') ;;
  esac
  window=$(printf '%s' "$line" | sed -n 's/.*"window":"\([^"]*\)".*/\1/p')
  if [ -n "$window" ] && [ -f "$dir/$key@$window.1" ]; then key="$key@$window"; fi
  n=$(cat "$dir/$key.n" 2>/dev/null || echo 0)
  if [ -f "$dir/$key.$((n + 1))" ]; then n=$((n + 1)); echo "$n" > "$dir/$key.n"; fi
  if [ -f "$dir/$key.$n" ]; then
    sed "s/\"id\":0,/\"id\":$id,/" "$dir/$key.$n"
  else
    printf '{"jsonrpc":"2.0","id":%s,"result":{"isError":true,"content":[{"type":"text","text":"no reply for %s"}]}}\n' "$id" "$key"
  fi
done
"#;

/// The hidden workspace every window of the fake's agent is on.
const WORKSPACE: &str = "special:agent-test";

fn launch_tool() -> Value {
    json!({"name": "launch", "inputSchema": {"type": "object", "properties": {"command": {"type": "array"}}}})
}

fn named(address: &str, class: &str, title: &str) -> Value {
    json!({"address": address, "class": class, "title": title, "workspace": WORKSPACE, "xwayland": false})
}

fn window(xwayland: bool) -> Value {
    json!({"address": "0xabc", "class": "org.gnome.TextEditor", "title": "New Document", "workspace": WORKSPACE,
        "xwayland": xwayland})
}

fn ok(value: &Value) -> Value {
    json!({"jsonrpc": "2.0", "id": 0, "result": {"content": [{"type": "text", "text": value.to_string()}]}})
}

/// A list result as deskpilot's MCP server sends one: a text block per item, and the list itself.
fn list(items: &[Value]) -> Value {
    let content: Vec<Value> = items.iter().map(|item| json!({"type": "text", "text": item.to_string()})).collect();
    json!({"jsonrpc": "2.0", "id": 0, "result": {"content": content, "structuredContent": {"result": items}}})
}

fn refused(code: &str, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": 0, "result": {
        "isError": true,
        "structuredContent": {"code": code, "message": message},
        "content": [{"type": "text", "text": message}],
    }})
}

/// A fake deskpilot in its own directory, offering `tools`, whose replies are set per tool.
struct Fake {
    program: PathBuf,
}

impl Fake {
    fn new(name: &str, tools: &[Value]) -> Fake {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("desktop").join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let program = dir.join("deskpilot");
        std::fs::write(&program, SCRIPT).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        let fake = Fake { program };
        let ready = json!({"jsonrpc": "2.0", "id": 0, "result": {"protocolVersion": "2025-06-18", "capabilities": {}}});
        fake.replies("initialize", [ready]);
        fake.replies("tools", [json!({"jsonrpc": "2.0", "id": 0, "result": {"tools": tools}})]);
        fake
    }

    /// `replies` to `key`'s requests in order after any set before, the last repeated.
    fn replies(&self, key: &str, replies: impl IntoIterator<Item = Value>) -> &Fake {
        let dir = self.program.parent().unwrap();
        let first = (1..).find(|n| !dir.join(format!("{key}.{n}")).exists()).unwrap();
        for (n, reply) in (first..).zip(replies) {
            std::fs::write(dir.join(format!("{key}.{n}")), format!("{reply}\n")).unwrap();
        }
        self
    }

    fn path(&self) -> &str {
        self.program.to_str().unwrap()
    }

    fn dir(&self) -> &Path {
        self.program.parent().unwrap()
    }

    fn requests(&self) -> Vec<Value> {
        requests(&self.program)
    }
}

fn requests(program: &Path) -> Vec<Value> {
    let log = std::fs::read_to_string(program.with_file_name("requests")).unwrap();
    log.lines().map(|line| serde_json::from_str(line).unwrap()).collect()
}

/// The requests that called `tool`.
fn calls(fake: &Fake, tool: &str) -> Vec<Value> {
    fake.requests().into_iter().filter(|request| request.pointer("/params/name") == Some(&json!(tool))).collect()
}

fn command() -> Vec<String> {
    vec!["gnome-text-editor".into(), "--new-window".into()]
}

/// One app's text editor, launched and listed on the agent's workspace.
fn editor(name: &str, tools: &[Value]) -> Fake {
    let fake = Fake::new(name, tools);
    fake.replies("launch", [ok(&window(false))]).replies("windows", [list(&[window(false)])]);
    fake
}

async fn client(fake: &Fake) -> Arc<Deskpilot> {
    connect(fake.path(), View::Hidden).await.unwrap()
}

async fn open(fake: &Fake) -> DeskpilotPage {
    DeskpilotPage::open(client(fake).await, &[command()], View::Hidden).await.unwrap()
}

async fn refusal(fake: &Fake) -> OpenError {
    DeskpilotPage::open(client(fake).await, &[command()], View::Hidden).await.err().unwrap()
}

#[tokio::test]
async fn a_desktop_run_launches_its_app_as_a_named_deskpilot_agent() {
    let fake = editor("launches", &[launch_tool()]);
    fake.replies("wait_for", [ok(&window(false))]);
    let page = open(&fake).await;
    page.address().await.unwrap();
    let launch = calls(&fake, "launch");
    assert_eq!(launch[0].pointer("/params/arguments/command"), Some(&json!(["gnome-text-editor", "--new-window"])));
    // The page drives the window `launch` returned.
    assert_eq!(fake.requests().last().unwrap().pointer("/params/arguments/window"), Some(&json!("0xabc")));
    // deskpilot's name rule: up to 32 letters, digits, `-`, `_` or `.`.
    let agent = std::fs::read_to_string(fake.dir().join("agent")).unwrap();
    let agent = agent.trim_end();
    assert!(!agent.is_empty() && agent.len() <= 32, "{agent:?}");
    assert!(agent.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)), "{agent:?}");
}

#[tokio::test]
async fn a_desktop_run_on_a_deskpilot_without_launch_is_unavailable() {
    let fake = Fake::new("no-launch", &[]);
    let error = refusal(&fake).await;
    assert!(matches!(error, OpenError::Unavailable(_)), "{error:?}");
}

#[tokio::test]
async fn a_desktop_app_that_exits_before_a_window_is_invalid() {
    let fake = Fake::new("refused", &[launch_tool()]);
    fake.replies("launch", [refused("refused", "gnome-text-editor exited before it opened a window")]);
    let OpenError::Invalid(message) = refusal(&fake).await else { panic!("not invalid") };
    assert!(message.contains("exited before it opened a window"), "{message}");
}

#[tokio::test]
async fn a_desktop_app_on_xwayland_is_invalid() {
    let fake = Fake::new("xwayland", &[launch_tool()]);
    fake.replies("launch", [ok(&window(true))]);
    let error = refusal(&fake).await;
    assert!(matches!(error, OpenError::Invalid(_)), "{error:?}");
}

/// A window whose title turns to "Ready" on read `after`, and whose tree then keeps changing when `continuous`.
fn waiting_window(name: &str, after: u64, continuous: bool) -> Fake {
    let root = json!({"id":"e1", "role":"frame", "snapshot":"s1", "states":["showing"]});
    let fake = editor(name, &[launch_tool()]);
    fake.replies("wait_for", [ok(&window(false))]).replies("elements", [ok(&root)]);
    for read in 0..70 {
        let mut current = window(false);
        if read >= after {
            current["title"] = json!("Ready");
        }
        let mut root = root.clone();
        if continuous && read >= after {
            root["name"] = json!(format!("Progress {read}"));
        }
        fake.replies("wait_for", [ok(&current)]).replies("elements", [ok(&root)]);
    }
    fake
}

#[tokio::test]
async fn a_desktop_wait_uses_the_supplied_remaining_time() {
    let fake = waiting_window("wait-budget", 60, false);
    let page = open(&fake).await;
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
    let fake = waiting_window("wait-unlimited", 60, false);
    let page = open(&fake).await;
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
    let fake = waiting_window("wait-busy", 2, true);
    let page = open(&fake).await;
    let before = page.observe().await.unwrap();
    let wait = Action::new(Operation::Wait);
    let result = tokio::time::timeout(std::time::Duration::from_millis(1500), page.wait(&wait, &before, 4.0))
        .await
        .expect("settling a changed window must not consume the remaining run")
        .unwrap();
    assert!(result.page_changed, "{result:?}");
}

/// A `shot` reply naming a PNG the fake wrote beside itself, holding `bytes`.
fn shot(fake: &Fake, name: &str, bytes: &[u8]) -> Value {
    let path = fake.dir().join(format!("{name}.png"));
    std::fs::write(&path, bytes).unwrap();
    ok(&json!({"shot": name, "path": path, "width": 640, "height": 480}))
}

fn shot_tool() -> Value {
    json!({"name": "shot", "inputSchema": {"type": "object", "properties": {"hidden": {}}}})
}

/// A filmed page whose window never changes: a frame when the app opens and one after each action.
fn filmed(name: &str) -> Fake {
    let root = json!({"id":"e1", "role":"frame", "snapshot":"s1", "states":["showing"]});
    let fake = editor(name, &[launch_tool(), shot_tool()]);
    fake.replies("wait_for", [ok(&window(false))]).replies("elements", [ok(&root)]);
    fake.replies("shot", [shot(&fake, "launched", b"launched"), shot(&fake, "waited", b"waited")]);
    fake
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
    let fake = filmed("filmed");
    let (film, frames) = frames();
    let page = open(&fake).await.filmed(film).await;
    // An app can open on a secret it shows, so even the launch frame waits for an observation to clear it.
    assert!(frames.frames.lock().unwrap().is_empty());
    let before = page.observe().await.unwrap();
    // The agent's reading of that observation: no secret shows, so the frame held from launch is filmed.
    page.withhold_frames(false);
    page.wait(&Action::new(Operation::Wait), &before, 0.0).await.unwrap();
    // The frame after an action waits for the next observation, as the agent reads it, to show no secret.
    assert_eq!(*frames.frames.lock().unwrap(), [b"launched".to_vec()]);
    page.withhold_frames(false);
    assert_eq!(*frames.frames.lock().unwrap(), [b"launched".to_vec(), b"waited".to_vec()]);
    let shots = calls(&fake, "shot");
    // Shot in the run's own view: a hidden window is framed where it is, never brought on screen.
    assert!(shots.iter().all(|shot| shot.pointer("/params/arguments/hidden") == Some(&json!(true))), "{shots:?}");
}

#[tokio::test]
async fn a_filmed_desktop_run_takes_no_frame_while_frames_are_withheld() {
    let fake = filmed("withheld");
    let (film, frames) = frames();
    let page = open(&fake).await.filmed(film).await;
    let before = page.observe().await.unwrap();
    // The agent's reading of that observation: no secret shows, so the frame held from launch is filmed.
    page.withhold_frames(false);
    // As the agent does before typing a secret: the window may show it next, and a frame cannot be masked.
    page.withhold_frames(true);
    page.wait(&Action::new(Operation::Wait), &before, 0.0).await.unwrap();
    page.withhold_frames(false);
    assert_eq!(*frames.frames.lock().unwrap(), [b"launched".to_vec()]);
    assert_eq!(calls(&fake, "shot").len(), 1);
}

#[tokio::test]
async fn a_filmed_click_marks_its_element_centre_for_the_recording_cursor() {
    let button = json!({"id":"e2", "role":"push button", "name":"Save", "states":["showing", "enabled"],
        "x": 100, "y": 40, "width": 80, "height": 20});
    let root = json!({"id":"e1", "role":"frame", "snapshot":"s1", "states":["showing"], "width": 800, "height": 600,
        "children": [button]});
    // The button's own label changes, which the observation shows.
    let mut saved = root.clone();
    saved["children"][0]["name"] = json!("Saved");
    let fake = editor("pointed", &[launch_tool(), shot_tool()]);
    fake.replies("wait_for", [ok(&window(false))]).replies("elements", [ok(&root), ok(&saved)]);
    fake.replies("click", [ok(&json!({"x": 140, "y": 50}))]);
    fake.replies("shot", [shot(&fake, "launched", b"launched"), shot(&fake, "clicked", b"clicked")]);
    let (film, kept) = frames();
    let page = open(&fake).await.filmed(film).await;
    let before = page.observe().await.unwrap();
    // The agent's reading of that observation: no secret shows, so the frame held from launch is filmed.
    page.withhold_frames(false);
    let mut click = Action::new(Operation::Click);
    click.target_id = Some("e2".into());
    let result = page.act(&click, &before).await.unwrap();
    assert!(result.page_changed, "{result:?}");
    let points = kept.points.lock().unwrap();
    let points: Vec<_> = points.iter().map(|m| (m.operation, m.x, m.y, m.width, m.height)).collect();
    assert_eq!(points, [(Operation::Click, 140.0, 50.0, 800.0, 600.0)]);
    page.withhold_frames(false);
    assert_eq!(*kept.frames.lock().unwrap(), [b"launched".to_vec(), b"clicked".to_vec()]);
}

#[tokio::test]
async fn a_frame_after_an_action_that_reveals_a_secret_is_never_filmed() {
    let fake = filmed("revealed");
    let (film, frames) = frames();
    let page = open(&fake).await.filmed(film).await;
    let before = page.observe().await.unwrap();
    // The agent's reading of that observation: no secret shows, so the frame held from launch is filmed.
    page.withhold_frames(false);
    page.wait(&Action::new(Operation::Wait), &before, 0.0).await.unwrap();
    // The next observation shows a resolved secret the action put on screen: the shot taken after it is dropped,
    // and a later clean reading does not bring it back.
    page.withhold_frames(true);
    page.withhold_frames(false);
    assert_eq!(*frames.frames.lock().unwrap(), [b"launched".to_vec()]);
}

#[tokio::test]
async fn a_film_frame_leaves_no_png_in_deskpilots_cache() {
    let program = filmed("frames-on-disk");
    let (film, _) = frames();
    let page =
        DeskpilotPage::open(program.to_str().unwrap(), &command(), View::Hidden).await.unwrap().filmed(film).await;
    let before = page.observe().await.unwrap();
    page.withhold_frames(false);
    page.wait(&Action::new(Operation::Wait), &before, 0.0).await.unwrap();
    // The frame after the action is held, and the agent then rejects it: neither frame's file may remain.
    page.withhold_frames(true);
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("desktop").join("frames-on-disk-shots");
    assert!(!dir.join("launched.png").exists() && !dir.join("waited.png").exists());
}

fn stop_tool() -> Value {
    json!({"name": "stop", "inputSchema": {"type": "object", "properties": {}}})
}

/// #1345: a run's apps outlived it, since the server was killed before deskpilot could close them.
#[tokio::test]
async fn closing_a_desktop_run_stops_its_apps_before_the_server_ends() {
    let fake = editor("closed", &[launch_tool(), stop_tool()]);
    fake.replies("stop", [ok(&json!("ok"))]);
    let client = client(&fake).await;
    let _page = DeskpilotPage::open(client.clone(), &[command()], View::Hidden).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), client.close())
        .await
        .expect("a server that reads the end of its input exits, without waiting to be killed")
        .unwrap();
    let names: Vec<_> = fake.requests().iter().filter_map(|r| r.pointer("/params/name").cloned()).collect();
    assert_eq!(names.last(), Some(&json!("stop")), "{names:?}");
    // A second close, as a run that ended and was then cancelled makes, sends nothing more.
    client.close().await.unwrap();
    assert_eq!(calls(&fake, "stop").len(), 1);
}

fn calculator() -> Value {
    named("0xdef", "org.gnome.Calculator", "Calculator")
}

fn frame(snapshot: &str) -> Value {
    json!({"id":"e1", "role":"frame", "snapshot": snapshot, "states":["showing"]})
}

/// Two apps of one run: a text editor and a calculator, each answering for its own window.
fn two_apps(name: &str) -> Fake {
    let fake = Fake::new(name, &[launch_tool()]);
    fake.replies("launch", [ok(&window(false)), ok(&calculator())]);
    fake.replies("wait_for@0xabc", [ok(&window(false))]).replies("elements@0xabc", [ok(&frame("s1"))]);
    fake.replies("wait_for@0xdef", [ok(&calculator())]).replies("elements@0xdef", [ok(&frame("s2"))]);
    fake
}

async fn open_both(fake: &Fake) -> DeskpilotPage {
    let calculator = vec!["gnome-calculator".to_string()];
    DeskpilotPage::open(client(fake).await, &[command(), calculator], View::Hidden).await.unwrap()
}

fn tabs(observation: &fastbrowse_core::page::Observation) -> Vec<(&str, bool)> {
    observation.tabs.iter().map(|tab| (tab.id.as_str(), tab.active)).collect()
}

#[tokio::test]
async fn every_app_of_a_desktop_run_is_a_tab_and_switching_moves_the_page_to_its_window() {
    let fake = two_apps("switched");
    let mut elsewhere = named("0x999", "other", "Not the run's");
    elsewhere["workspace"] = json!("1");
    fake.replies("windows", [list(&[window(false), calculator(), elsewhere])]);
    let page = open_both(&fake).await;
    let launched: Vec<_> = calls(&fake, "launch").iter().map(|r| r["params"]["arguments"]["command"].clone()).collect();
    assert_eq!(launched, [json!(["gnome-text-editor", "--new-window"]), json!(["gnome-calculator"])]);
    let before = page.observe().await.unwrap();
    // Only windows on the run's own workspace are its tabs; the first app's is active.
    assert_eq!(tabs(&before), [("0xabc", true), ("0xdef", false)]);
    let mut switch = Action::new(Operation::SwitchTab);
    switch.tab_id = Some("0xdef".into());
    let result = page.act(&switch, &before).await.unwrap();
    assert_eq!((result.outcome, result.page_changed), (StepOutcome::Executed, true), "{result:?}");
    let after = page.observe().await.unwrap();
    assert_eq!(tabs(&after), [("0xabc", false), ("0xdef", true)]);
    assert_eq!(after.document_key, "0xdef");
    let read = calls(&fake, "elements");
    assert_eq!(read.last().unwrap().pointer("/params/arguments/window"), Some(&json!("0xdef")));
    // Switching moves only what the page reads: nothing on the desktop is focused or brought into view.
    assert!(calls(&fake, "focus").is_empty());
}

#[tokio::test]
async fn a_switch_to_a_window_that_is_not_the_runs_is_refused() {
    let fake = two_apps("switch-refused");
    let mut elsewhere = named("0x999", "other", "Not the run's");
    elsewhere["workspace"] = json!("1");
    fake.replies("windows", [list(&[window(false), calculator(), elsewhere])]);
    let page = open_both(&fake).await;
    let before = page.observe().await.unwrap();
    assert_eq!(tabs(&before), [("0xabc", true), ("0xdef", false)]);
    let mut switch = Action::new(Operation::SwitchTab);
    switch.tab_id = Some("0x999".into());
    let result = page.act(&switch, &before).await.unwrap();
    assert_eq!(result.outcome, StepOutcome::Failed, "{result:?}");
}

#[tokio::test]
async fn a_window_an_action_opens_becomes_the_active_tab() {
    let fake = two_apps("followed");
    let dialog = named("0x123", "org.gnome.TextEditor", "Save As");
    let button = json!({"id":"e2", "role":"push button", "name":"Save", "states":["showing", "enabled"]});
    let root = json!({"id":"e1", "role":"frame", "snapshot":"s1", "states":["showing"], "children": [button]});
    fake.replies("elements@0xabc", [ok(&root)]);
    fake.replies("wait_for@0x123", [ok(&dialog)]).replies("elements@0x123", [ok(&frame("s3"))]);
    fake.replies("click", [ok(&json!({"x": 1, "y": 1}))]);
    // Listed before the click, then with the dialog the click opened.
    fake.replies("windows", [list(&[window(false), calculator()]), list(&[window(false), calculator(), dialog])]);
    let page = open_both(&fake).await;
    let before = page.observe().await.unwrap();
    let mut click = Action::new(Operation::Click);
    click.target_id = Some("e2".into());
    let result = page.act(&click, &before).await.unwrap();
    // The editor's own window did not change; the dialog is the change.
    assert!(result.page_changed, "{result:?}");
    let after = page.observe().await.unwrap();
    assert_eq!(tabs(&after), [("0xabc", false), ("0xdef", false), ("0x123", true)]);
    assert_eq!(after.title, "Save As");
}

#[tokio::test]
async fn a_wait_ends_when_a_window_opens_though_the_active_windows_tree_stays_as_it_was() {
    let fake = two_apps("wait-opened");
    let dialog = named("0x123", "org.gnome.TextEditor", "Save As");
    fake.replies("wait_for@0x123", [ok(&dialog)]).replies("elements@0x123", [ok(&frame("s3"))]);
    fake.replies("windows", [list(&[window(false), calculator()]), list(&[window(false), calculator(), dialog])]);
    let page = open_both(&fake).await;
    let before = page.observe().await.unwrap();
    let wait = Action::new(Operation::Wait);
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), page.wait(&wait, &before, f64::INFINITY))
        .await
        .expect("an opened window ends the wait")
        .unwrap();
    assert!(result.page_changed, "{result:?}");
    let after = page.observe().await.unwrap();
    assert_eq!(after.title, "Save As");
}

#[tokio::test]
async fn when_the_active_window_closes_the_one_active_before_it_is_active_again() {
    let fake = two_apps("fallback");
    fake.replies("wait_for@0xdef", [refused("not_found", "no window 0xdef")]);
    // Both open for the first observation, the switch and the calculator's observation, then the calculator closed.
    let both = list(&[window(false), calculator()]);
    fake.replies("windows", [both.clone(), both.clone(), both, list(&[window(false)])]);
    let page = open_both(&fake).await;
    let before = page.observe().await.unwrap();
    let mut switch = Action::new(Operation::SwitchTab);
    switch.tab_id = Some("0xdef".into());
    page.act(&switch, &before).await.unwrap();
    assert_eq!(page.observe().await.unwrap().document_key, "0xdef");
    let after = page.observe().await.unwrap();
    assert_eq!(after.document_key, "0xabc");
    assert_eq!(tabs(&after), [("0xabc", true)]);
}

#[tokio::test]
async fn a_run_whose_every_window_closed_ends_its_page() {
    let fake = editor("all-closed", &[launch_tool()]);
    fake.replies("wait_for", [refused("not_found", "no window 0xabc")]).replies("windows", [list(&[])]);
    let page = open(&fake).await;
    let error = page.observe().await.unwrap_err();
    assert!(matches!(error, BrowserError::Unavailable(_)), "{error:?}");
}

/// #1345: two windows with one title, such as an app an earlier run left open, made deskpilot refuse to pick a tree,
/// and the run called that an outage.
#[tokio::test]
async fn a_window_deskpilot_cannot_tell_apart_by_title_fails_the_read_without_calling_it_an_outage() {
    let fake = editor("ambiguous", &[launch_tool()]);
    let message = "Could not choose an accessibility toplevel for 'New Document'";
    fake.replies("wait_for", [ok(&window(false))]).replies("elements", [refused("ambiguous", message)]);
    let page = open(&fake).await;
    let BrowserError::Failed(error) = page.observe().await.unwrap_err() else { panic!("an outage") };
    assert!(error.contains("same title") && error.contains(message), "{error}");
}
