//! Moving the destination out from under the pointer does not authorize a release onto an overlay.

mod common;

use common::{control, eval_active, harness};
use fastbrowse_core::models::{Operation, StepOutcome};
use fastbrowse_core::page::Action;
use serde_json::json;

#[tokio::test(flavor = "multi_thread")]
async fn test_a_pointer_drag_does_not_release_on_an_overlay_when_the_destination_slides_away() {
    let h = harness().await.expect("Chrome is required for this regression test");
    h.goto("/dispatch.html").await;
    eval_active(
        &h.session,
        r#"document.body.innerHTML = `
          <div id="source" style="position:fixed;left:20px;top:20px;width:100px;height:80px;cursor:grab">Card A</div>
          <button id="drop" style="position:fixed;left:220px;top:20px;width:100px;height:80px">Done</button>`;
        window.drops = [];
        let held = false;
        document.getElementById('source').addEventListener('pointerdown', () => { held = true; });
        document.addEventListener('pointermove', event => {
          if (held && event.clientX > 150 && !window.changed) {
            window.changed = true;
            document.getElementById('drop').style.left = '321px';
            document.body.insertAdjacentHTML('beforeend',
              '<button id="zone" style="position:fixed;left:250px;top:40px;width:40px;height:40px;z-index:10">Trash</button>');
          }
        });
        document.addEventListener('pointerup', event => {
          if (held) {
            const hit = document.elementFromPoint(event.clientX, event.clientY);
            if (hit && hit.id !== 'source') window.drops.push(hit.textContent);
          }
          held = false;
        }); true"#,
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
    assert!(drops == json!(["Done"]) || drops == json!([]), "release hit an unchosen control: {drops:?}, {result:?}");
    let expected = if drops == json!([]) { StepOutcome::Stale } else { StepOutcome::Executed };
    assert_eq!(result.outcome, expected, "{result:?}");
}
