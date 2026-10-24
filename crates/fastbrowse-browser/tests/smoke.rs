// The whole path once: open a tab, navigate, observe the main frame and its cross-origin iframe, capture both.

mod common;

use common::{harness, sites};

#[tokio::test(flavor = "multi_thread")]
async fn observes_and_captures_the_fixture_page_with_its_cross_origin_frame() {
    let Some(h) = harness().await else { return };
    h.goto("/index.html").await;
    let observation = h.observe().await;
    assert!(observation.url.starts_with(&sites().main));
    // The iframe is served from another site, so its controls come from a session of its own.
    assert!(observation.controls.iter().any(|c| c.label == "Frame button" && c.frame_id.is_some()));
    let capture = h.page.capture().await.expect("capture");
    assert!(capture.text.contains("Inside the frame"));
    h.close().await;
}
