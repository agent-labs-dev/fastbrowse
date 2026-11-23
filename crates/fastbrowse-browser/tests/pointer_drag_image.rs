//! A grab cursor on an image names a drag handle rather than a surface to draw inside.

mod common;

use common::{control, eval_active, harness};
use fastbrowse_core::models::Operation;

#[tokio::test(flavor = "multi_thread")]
async fn test_an_image_handle_can_be_dragged_onto_another_control() {
    let Some(h) = harness().await else { return };
    h.goto("/dispatch.html").await;
    eval_active(
        &h.session,
        r#"document.body.innerHTML = `
          <img alt="Card A" draggable="false"
            src="data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' width='100' height='80'%3E%3Crect width='100' height='80' fill='blue'/%3E%3C/svg%3E"
            style="width:100px;height:80px;cursor:grab">
          <button>Card B</button>`; true"#,
    )
    .await;
    let observation = h.observe().await;
    h.close().await;
    let source = control(&observation, "Card A");
    assert!(source.operations.contains(&Operation::Drag), "{source:?}");
    assert_ne!(source.drawn, Some(true), "a drawn control never gets a destination choice: {source:?}");
}
