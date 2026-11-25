//! The CurseForge author console journeys, driven step by step on local copies of its pages (`description-editor`,
//! `addon-comments` and `hover-gallery` under `tests/browser/sites/main`). Signed-in `--extension` runs on the real
//! console failed where every launched-Chrome fixture passed (#1414), so these journeys run on both paths: the browser
//! crate's tests drive them over CDP, and `fastbrowse-cli/tests/extension_backend.rs` through the extension.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use fastbrowse_browser::page::CdpPage;
use fastbrowse_browser::session::BrowserSession;
use fastbrowse_core::models::{Operation, StepOutcome};
use fastbrowse_core::page::{Action, Control, Observation};
use serde_json::{Value, json};

const DESCRIPTION: &str = "# Shortest Path Forever\n\nThe quickest walking route to your next objective.\n\n\
                           ## Settings\n\nType /spf to open the settings panel.";
const REPLY: &str = "Thanks, the next version prefers the tunnel.";
const REPLY_2: &str = "Thanks, the arrow now skips the harbor.";

async fn eval(page: &CdpPage, session: &BrowserSession, expression: &str) -> Value {
    let session_id = session.active_session_id().expect("active session");
    page.evaluate(&session_id, expression).await.expect("evaluate")
}

async fn eval_until(page: &CdpPage, session: &BrowserSession, expression: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while eval(page, session, expression).await != json!(true) {
        assert!(Instant::now() < deadline, "never true: {expression}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn act(page: &CdpPage, operation: Operation, target: &Control, text: Option<&str>, observation: &Observation) {
    let action = Action { target_id: Some(target.id.clone()), text: text.map(str::to_owned), ..Action::new(operation) };
    let result = page.act(&action, observation).await.expect("act");
    assert_eq!(result.outcome, StepOutcome::Executed, "{operation:?} {:?}: {result:?}", target.label);
}

fn find<'a>(observation: &'a Observation, matches: impl Fn(&Control) -> bool, what: &str) -> &'a Control {
    observation.controls.iter().find(|c| matches(c)).unwrap_or_else(|| {
        let seen: Vec<_> = observation.controls.iter().map(|c| (&c.label, &c.context)).collect();
        panic!("no {what} in {seen:?}")
    })
}

/// Replace the stored description and save it: the whole text is replaced, nothing appended, and Save, disabled
/// until the text changes, is what stores it.
pub async fn description(page: &CdpPage, session: &BrowserSession, base: &str) {
    page.navigate(&format!("{base}/description-editor.html"), None).await.expect("navigate");
    // The copy is faithful only if it refuses what React refuses: a value assigned by script, then `input`.
    eval(
        page,
        session,
        "const t = document.querySelector('.mde-text'); t.value = 'scripted'; \
         t.dispatchEvent(new Event('input', { bubbles: true }))",
    )
    .await;
    assert_eq!(eval(page, session, "state.value === state.stored").await, json!(true));
    eval(page, session, "render()").await;
    let observation = page.observe().await.expect("observe");
    // Save is on the page before any edit, disabled and offering nothing: left out, a Save that stayed disabled after
    // a fill read as no Save at all (#1415).
    let unsaved = find(&observation, |c| c.label == "Save", "the disabled Save");
    assert!(unsaved.disabled == Some(true) && unsaved.operations.is_empty(), "{unsaved:?}");
    let editor = find(&observation, |c| c.label == "Description" && c.role == "textbox", "description editor");
    act(page, Operation::Fill, editor, Some(DESCRIPTION), &observation).await;
    assert_eq!(eval(page, session, "state.value").await, json!(DESCRIPTION), "the edit reached the page's state");
    let filled = page.observe().await.expect("observe");
    let told = fastbrowse_core::effects::effect(&observation, &filled, None, false).summary;
    assert!(told.contains("Save disabled: True -> False"), "the fill's effect says the page took it: {told}");
    let observation = filled;
    let save = find(&observation, |c| c.label == "Save" && c.operations.contains(&Operation::Click), "an enabled Save");
    act(page, Operation::Click, save, None, &observation).await;
    eval_until(page, session, "state.stored === state.value && !state.saving").await;
    assert_eq!(eval(page, session, "state.stored").await, json!(DESCRIPTION));
    assert_eq!(eval(page, session, "document.querySelector('.form-actions .btn-primary').disabled").await, json!(true));
}

/// Reply to two comments, each in its own thread: Reply opens an editor inside that comment, prefilled with the
/// author's name, beside the page's own comment box. Nivlemoth_Nathrezim also replied in another thread and IronmanBG
/// wrote twice, so each name link shows twice and names no comment: the editor and its Post are told apart by the
/// commenter and the comment's age instead, and the page's own box is named as outside every comment listed.
pub async fn reply(page: &CdpPage, session: &BrowserSession, base: &str) {
    reply_to(page, session, base, "Nivlemoth_Nathrezim", "7 hours ago", "over the mountain in Alterac", REPLY).await;
    reply_to(page, session, base, "IronmanBG", "The arrow also points", "The arrow also points the wrong way", REPLY_2)
        .await;
}

async fn reply_to(
    page: &CdpPage,
    session: &BrowserSession,
    base: &str,
    author: &str,
    card: &str,
    parent: &str,
    text: &str,
) {
    page.navigate(&format!("{base}/addon-comments.html"), None).await.expect("navigate");
    let observation = page.observe().await.expect("observe");
    let named = |c: &Control| c.context.as_deref().is_some_and(|context| context.contains(author));
    let own_box = find(&observation, |c| c.role == "textbox" && !named(c), "the page's own comment box");
    assert!(
        own_box.context.as_deref().is_some_and(|context| context.contains("outside every item listed")),
        "the page's own box is named as outside every comment: {own_box:?}"
    );
    let reply = find(
        &observation,
        |c| c.label == "Reply" && c.context.as_deref().is_some_and(|context| context.contains(card) && named(c)),
        "Reply on the comment",
    );
    assert!(named(reply), "Reply is named by its commenter: {reply:?}");
    act(page, Operation::Click, reply, None, &observation).await;
    let before: HashSet<&str> = observation.controls.iter().map(|c| c.id.as_str()).collect();
    let opened = page.observe().await.expect("observe");
    // The reply editor and its Post are the controls Reply added; the page's own box was there before.
    let added = |c: &Control| !before.contains(c.id.as_str());
    let editor = find(&opened, |c| added(c) && c.role == "textbox", "the opened reply editor");
    assert!(named(editor), "the opened editor is named by the comment: {editor:?}");
    act(page, Operation::Fill, editor, Some(text), &opened).await;
    let filled = page.observe().await.expect("observe");
    let post = find(&filled, |c| added(c) && c.label == "Post", "the reply editor's Post");
    assert!(named(post), "Post is named by the comment: {post:?}");
    act(page, Operation::Click, post, None, &filled).await;
    let placed = format!(
        "[...document.querySelectorAll('#thread > li')].map(li => [li.querySelector('.text').textContent, \
         [...li.querySelectorAll(':scope .comments-list .text')].map(t => t.textContent)])\
         .filter(([, replies]) => replies.includes({}))",
        json!(text)
    );
    let everywhere =
        format!("[...document.querySelectorAll('.text')].filter(t => t.textContent === {}).length", json!(text));
    assert_eq!(eval(page, session, &everywhere).await, json!(1), "posted exactly once");
    let threads = eval(page, session, &placed).await;
    assert_eq!(threads.as_array().map(Vec::len), Some(1), "posted once, as a reply: {threads}");
    assert!(threads[0][0].as_str().unwrap().contains(parent), "{threads}");
}

/// Delete one gallery image through the controls its card shows only while the pointer is over it.
pub async fn gallery(page: &CdpPage, session: &BrowserSession, base: &str) {
    page.navigate(&format!("{base}/hover-gallery.html"), None).await.expect("navigate");
    let observation = page.observe().await.expect("observe");
    let card = find(
        &observation,
        |c| c.operations.contains(&Operation::Hover) && c.label == "The quest log grouped by zone",
        "a hover target for the second card",
    );
    act(page, Operation::Hover, card, None, &observation).await;
    let revealed = page.observe().await.expect("observe");
    let delete = find(
        &revealed,
        |c| c.label == "delete" && c.context.as_deref() == Some("The quest log grouped by zone"),
        "the second card's delete",
    );
    act(page, Operation::Click, delete, None, &revealed).await;
    assert_eq!(eval(page, session, "document.getElementById('status').textContent").await, json!("Deleted m2"));
}
