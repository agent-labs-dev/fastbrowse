mod common;

use common::{chrome, eval_active, find, harness_on, turn};
use fastbrowse_browser::session::SessionOptions;
use fastbrowse_core::config::Config;
use fastbrowse_core::models::{BrowserConnection, Operation, StepOutcome};
use fastbrowse_core::page::Action;

/// A replacement tab must not let sandboxed content open a popup Chrome would refuse.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_background_link_respects_the_frames_popup_sandbox() {
    let _turn = turn().await;
    let Some(connection) = chrome().cloned() else {
        eprintln!("Chrome is not available; skipping");
        return;
    };
    let background = BrowserConnection { foreground: false, ..connection };
    let h = harness_on(background, Config::default(), SessionOptions::default()).await.expect("harness");
    h.session
        .send("Target.activateTarget", serde_json::json!({"targetId": h.session.active_target_id()}), None)
        .await
        .expect("fixture current tab");
    h.goto("/popup.html").await;
    eval_active(
        &h.session,
        "document.body.innerHTML = '<iframe sandbox=\"allow-same-origin\" style=\"width:500px;height:250px\"></iframe>'; \
         const doc = document.querySelector('iframe').contentDocument; \
         doc.body.innerHTML = '<a style=\"display:block;width:200px;height:50px\" href=\"/popup.html\" \
         target=\"_blank\">Restricted link</a>'",
    )
    .await;
    let observation = h.observe().await;
    let opener = h.session.active_target_id();
    let tabs_before = h.session.tabs().len();
    let action =
        Action { target_id: Some(find(&observation, "Restricted link").id.clone()), ..Action::new(Operation::Click) };
    let result = h.page.act(&action, &observation).await.expect("act");
    assert_eq!(result.outcome, StepOutcome::Executed, "{result:?}");
    let tabs_after = h.session.tabs().len();
    let active_after = h.session.active_target_id();
    h.close().await;
    assert_eq!(tabs_after, tabs_before, "sandbox without allow-popups must not create a new tab: {result:?}");
    assert_eq!(active_after, opener);
}
