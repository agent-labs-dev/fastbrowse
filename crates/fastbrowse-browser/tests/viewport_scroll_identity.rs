//! A redraw of a scroll owner must not turn a place already shown into a new place.

mod common;

use common::{eval_active, harness, turn};
use fastbrowse_core::effects::state_key;

#[tokio::test(flavor = "multi_thread")]
async fn a_redrawn_scroll_owner_keeps_the_identity_of_a_place_already_shown() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    eval_active(
        &h.session,
        "document.body.innerHTML = '<div id=panel aria-label=Article style=\"height:200px;overflow:auto\"><div style=height:1600px>Article</div></div>'",
    )
    .await;
    let top = h.observe().await;
    assert!(top.viewport_text.contains("Article"));
    eval_active(&h.session, "document.querySelector('#panel').scrollTop=200").await;
    let below = h.observe().await;
    assert_ne!(below.viewport_key, top.viewport_key);

    // A framework can replace a panel while rendering its scrolled content. Its address, contents and scroll
    // position still identify the same place when the agent returns to the top.
    eval_active(
        &h.session,
        "const panel=document.querySelector('#panel');panel.scrollTop=0;panel.replaceWith(panel.cloneNode(true))",
    )
    .await;
    let returned = h.observe().await;
    assert_eq!(returned.viewport_text, top.viewport_text);
    assert_eq!(state_key(&returned), state_key(&top));
    assert_eq!(returned.viewport_key, top.viewport_key, "redrawing a scroll owner must not renew scroll progress");
    h.close().await;
}
