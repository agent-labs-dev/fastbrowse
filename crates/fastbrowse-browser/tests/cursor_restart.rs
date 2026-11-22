//! An attached document may outlive the PageCursor that drew on it.

mod common;

use common::{chrome, find, harness};
use fastbrowse_browser::page::CdpPage;
use fastbrowse_browser::session::BrowserSession;
use fastbrowse_core::models::{Operation, StepOutcome};
use fastbrowse_core::page::Action;

#[tokio::test(flavor = "multi_thread")]
async fn test_a_new_page_cursor_hides_a_cursor_already_in_the_document() {
    let h = harness().await.expect("Chrome is required for this regression test");
    h.goto("/canvas-drag.html").await;
    let plain = h.page.screenshot().await.unwrap();
    let mut connection = chrome().unwrap().clone();
    connection.attach = true;
    connection.target_match = Some("Canvas drag".into());
    let previous_session = BrowserSession::new(connection.clone(), h.sink.clone(), Default::default());
    previous_session.open().await.unwrap();
    let previous = CdpPage::new(previous_session.clone(), Default::default()).with_cursor();
    let observation = previous.observe().await.unwrap();
    let action =
        Action { target_id: Some(find(&observation, "Drawing pad").id.clone()), ..Action::new(Operation::Hover) };
    assert_eq!(previous.act(&action, &observation).await.unwrap().outcome, StepOutcome::Executed);
    let watched = h.page.screenshot().await.unwrap();
    assert!(previous.screenshot().await.unwrap() != watched, "the original controller must hide its cursor");
    previous_session.close().await.unwrap();
    drop(previous);
    assert!(h.page.screenshot().await.unwrap() == plain, "closing left the cursor in the attached tab");
    // Starting a second run on an attached document creates a new Rust cursor with no last position.
    let next_session = BrowserSession::new(connection, h.sink.clone(), Default::default());
    next_session.open().await.unwrap();
    let next = CdpPage::new(next_session.clone(), Default::default()).with_cursor();
    let for_model = next.screenshot().await.unwrap();
    assert!(for_model != watched, "the new run's model screenshot contains the previous cursor");
    next_session.close().await.unwrap();
    h.close().await;
}
