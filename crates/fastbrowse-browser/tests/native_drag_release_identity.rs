//! A native drag must retain the destination's observed identity until release.

mod common;

use common::{control, eval_active, harness};
use fastbrowse_core::models::{Operation, StepOutcome};
use fastbrowse_core::page::Action;
use serde_json::json;

#[tokio::test(flavor = "multi_thread")]
async fn test_a_native_drag_cancels_when_dragover_repurposes_the_destination() {
    let h = harness().await.expect("Chrome is required for this regression test");
    h.goto("/dispatch.html").await;
    eval_active(
        &h.session,
        r#"document.body.innerHTML = `
          <div id="source" draggable="true"
            style="position:fixed;left:20px;top:20px;width:100px;height:80px">Card A</div>
          <button id="drop" style="position:fixed;left:220px;top:20px;width:100px;height:80px">Done</button>`;
        window.drops = [];
        const source = document.getElementById('source');
        const drop = document.getElementById('drop');
        source.ondragstart = event => event.dataTransfer.setData('text/plain', 'Card A');
        drop.ondragover = event => {
          event.preventDefault();
          drop.textContent = 'Trash';
        };
        drop.ondrop = event => {
          event.preventDefault();
          window.drops.push(drop.textContent);
        }; true"#,
    )
    .await;
    let observation = h.observe().await;
    let source = control(&observation, "Card A");
    assert!(source.operations.contains(&Operation::Drag));
    let action = Action {
        target_id: Some(source.id.clone()),
        destination_id: Some(control(&observation, "Done").id.clone()),
        ..Action::new(Operation::Drag)
    };
    let result = h.page.act(&action, &observation).await.expect("act");
    let drops = eval_active(&h.session, "window.drops").await;
    h.close().await;
    assert_eq!(drops, json!([]), "the drag must not release on a repurposed destination: {result:?}");
    assert_eq!(result.outcome, StepOutcome::Stale, "{result:?}");
}
