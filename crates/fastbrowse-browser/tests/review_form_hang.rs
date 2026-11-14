mod common;
mod lifecycle_support;

use fastbrowse_browser::error::BrowserError;
use fastbrowse_browser::page::CdpPage;
use fastbrowse_core::config::Config;
use fastbrowse_core::models::Operation;
use fastbrowse_core::page::Action;
use lifecycle_support::FakeChrome;
use serde_json::json;

#[tokio::test(flavor = "multi_thread")]
async fn a_fill_preserves_an_unresponsive_renderer_error() {
    let fake = FakeChrome::start().await;
    let session = fake.session(fake.connection()).await;
    let page = CdpPage::new(session.clone(), Config::default());
    fake.queue(
        "Runtime.evaluate",
        [
            json!({"result": {"value": {
                "url": "https://example.test/", "title": "Form", "viewport_text": "Name",
                "document_key": "document", "page_key": "page",
                "controls": [{"id": 1, "role": "textbox", "label": "Name", "operations": ["fill"]}],
                "guards": {"1": "guard"}, "retarget_keys": {}
            }}}),
            json!({"result": {"value": {
                "url": "https://example.test/", "document_key": "document", "response_status": 200
            }}}),
        ],
    );
    let observation = page.observe().await.expect("initial observation");
    let action = Action {
        target_id: Some(observation.controls[0].id.clone()),
        text: Some("Alice".into()),
        ..Action::new(Operation::Fill)
    };
    let active = session.active_session_id().unwrap();
    // Seed the same renderer state reached after unanswered commands, without waiting two command deadlines.
    let client = session.client().unwrap();
    client.missed_deadline(&active);
    client.missed_deadline(&active);
    let probe = session.send("Runtime.evaluate", json!({"expression": "1"}), Some(&active)).await;
    assert!(matches!(probe, Err(BrowserError::PageUnresponsive(_))));
    let result = page.act(&action, &observation).await;
    session.close().await.unwrap();
    assert!(
        matches!(result, Err(BrowserError::PageUnresponsive(_))),
        "a hung renderer during a fill must reach the agent's recovery handler, got {result:?}"
    );
}
