// The whole path once: open a tab, navigate, observe the main frame and its cross-origin iframe, capture both.

mod common;

use common::{harness, sites};

#[tokio::test(flavor = "multi_thread")]
async fn observes_and_captures_the_fixture_page_with_its_cross_origin_frame() {
    let Some(h) = harness().await else { return };
    h.goto("/index.html").await;
    // The frame session attaches before its document loads, so observe until the frame's control is there.
    let observation = common::observe_until(&h.page, "Frame button").await;
    assert!(observation.url.starts_with(&sites().main));
    // The iframe is served from another site, so its controls come from a session of its own.
    assert!(observation.controls.iter().any(|c| c.label == "Frame button" && c.frame_id.is_some()));
    // The frame's text follows the page's own text under a line naming the frame's origin.
    let label = format!("[Frame: {}]", sites().iframe);
    let viewport = &observation.viewport_text;
    let at = viewport.find(&label).unwrap_or_else(|| panic!("no frame label in {viewport:?}"));
    assert!(viewport[at..].contains("Inside the frame"), "{viewport:?}");
    assert!(viewport.find("Fastbrowse fixture").is_some_and(|main| main < at), "{viewport:?}");
    let offscreen = &observation.offscreen_text;
    let at = offscreen.find(&label).unwrap_or_else(|| panic!("no frame label in {offscreen:?}"));
    assert!(offscreen[at..].contains("Below the frame fold"), "{offscreen:?}");
    let capture = h.page.capture().await.expect("capture");
    assert!(capture.text.contains("Inside the frame"));
    h.close().await;
}
