//! Preparing a screenshot of a surface must scroll its containing frame too.

mod common;

use common::{eval_active, find, harness, observe_until};
use serde_json::json;

#[tokio::test(flavor = "multi_thread")]
async fn test_reveal_scrolls_an_offscreen_frame_containing_a_visible_surface() {
    let h = harness().await.expect("Chrome is required for this regression test");
    h.goto("/canvas-drag.html").await;
    eval_active(&h.session, r#"document.body.innerHTML = `<div style="height:1600px">Before the drawing</div><iframe width="600" height="400" srcdoc='<canvas aria-label="Framed pad" width="400" height="200" onclick="void 0"></canvas>'></iframe>`"#).await;
    let observation = observe_until(&h.page, "Framed pad").await;
    let target = find(&observation, "Framed pad");
    assert_eq!(target.drawn, Some(true));
    h.page.reveal(&target.id).await.unwrap();
    assert!(
        h.page.edges(std::slice::from_ref(&target.id)).await.unwrap().contains_key(&target.id),
        "the screenshot needs the revealed surface bounds"
    );
    assert_eq!(eval_active(&h.session, "(() => { const r = document.querySelector('iframe').getBoundingClientRect(); return r.top < innerHeight && r.bottom > 0; })()").await, json!(true), "the drag-path model's screenshot still cannot see the target's frame");
    h.close().await;
}
