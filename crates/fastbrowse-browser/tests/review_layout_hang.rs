mod common;
mod lifecycle_support;

use std::time::Duration;

use fastbrowse_browser::error::BrowserError;
use fastbrowse_browser::page::CdpPage;
use fastbrowse_core::config::Config;
use fastbrowse_core::models::Operation;
use fastbrowse_core::page::Action;
use lifecycle_support::FakeChrome;
use serde_json::json;

#[tokio::test(flavor = "multi_thread")]
async fn layout_timeout_recovery_opens_a_fresh_tab() {
    let fake = FakeChrome::start().await;
    let session = fake.session(fake.connection()).await;
    let page = CdpPage::new(session.clone(), Config::default());
    fake.queue(
        "Runtime.evaluate",
        [
            json!({"result": {"value": {
                "url": "https://example.test/", "title": "Page", "viewport_text": "Go",
                "document_key": "document", "page_key": "page",
                "controls": [{"id": 1, "role": "button", "label": "Go", "operations": ["click"]}],
                "guards": {"1": "guard"}, "retarget_keys": {}
            }}}),
            json!({"result": {"value": {
                "url": "https://example.test/", "document_key": "document", "response_status": 200
            }}}),
        ],
    );
    let observation = page.observe().await.expect("initial observation");
    let action = Action { target_id: Some(observation.controls[0].id.clone()), ..Action::new(Operation::Click) };
    // The renderer stops after observation, before input is dispatched.
    fake.block("Runtime.evaluate");
    let error = tokio::time::timeout(Duration::from_secs(4), page.act(&action, &observation))
        .await
        .expect("layout deadline")
        .expect_err("hung layout");
    assert!(matches!(error, BrowserError::PageUnresponsive(_)), "{error:?}");
    assert!(!fake.calls().iter().any(|method| method.starts_with("Input.")));
    fake.queue("Target.createTarget", [json!({"targetId": "fresh"})]);
    fake.queue("Target.attachToTarget", [json!({"sessionId": "fresh-session"})]);
    fake.block("Page.navigate");
    // Recovery should probe for at most 10 seconds and create the replacement,
    // rather than queue a 120-second navigation behind the hung renderer.
    let _ = tokio::time::timeout(Duration::from_secs(14), page.navigate(&observation.url, None)).await;
    assert_eq!(
        fake.script().count("Target.createTarget"),
        2,
        "a layout PageUnresponsive must recover in a fresh tab even without a CDP command deadline"
    );
    session.close().await.unwrap();
}
