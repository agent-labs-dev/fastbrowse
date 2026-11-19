mod common;
mod lifecycle_support;

use fastbrowse_browser::page::CdpPage;
use fastbrowse_core::config::Config;
use fastbrowse_core::models::Operation;
use fastbrowse_core::page::Action;
use lifecycle_support::FakeChrome;
use serde_json::json;

/// A page can echo a typed secret as pixels the text check never sees (the field shows bullets, and a read that
/// follows can come back clean), so once a secret has been typed no frame of the run is delivered or recorded.
#[tokio::test(flavor = "multi_thread")]
async fn a_secret_fill_withholds_every_later_frame_whatever_a_reading_says() {
    let fake = FakeChrome::start().await;
    let session = fake.session(fake.connection()).await;
    let page = CdpPage::new(session.clone(), Config::default());
    fake.queue(
        "Runtime.evaluate",
        [
            json!({"result": {"value": {
                "url": "https://example.test/", "title": "Form", "viewport_text": "Code",
                "document_key": "document", "page_key": "page",
                "controls": [{"id": 1, "role": "textbox", "label": "Code", "operations": ["fill"]}],
                "guards": {"1": "guard"}, "retarget_keys": {}
            }}}),
            json!({"result": {"value": {
                "url": "https://example.test/", "document_key": "document", "response_status": 200
            }}}),
        ],
    );
    let observation = page.observe().await.unwrap();
    assert!(!session.frames_withheld());
    let measured = json!({"result": {"value": [
        "fingerprint", "guard", [100, 100], false, false, "https://example.test/", [1280, 720], true
    ]}});
    fake.queue("Runtime.evaluate", [measured.clone(), measured]);
    let action = Action {
        target_id: Some(observation.controls[0].id.clone()),
        text: Some("not-a-real-secret".into()),
        secret: true,
        ..Action::new(Operation::Fill)
    };
    // However the fill ends, the keystrokes were at least about to be sent.
    let _ = page.act(&action, &observation).await;
    assert!(session.frames_withheld());
    // A clean reading (the typed field is masked on the page) does not release them.
    page.withhold_frames(false);
    assert!(session.frames_withheld());
    session.close().await.unwrap();
}
