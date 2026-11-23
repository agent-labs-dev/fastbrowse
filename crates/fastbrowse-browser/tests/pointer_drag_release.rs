//! A destination with exposed edges is released on an exposed point, never on a control covering its centre.

mod common;

use common::{control, eval_active, harness};
use fastbrowse_core::models::{Operation, StepOutcome};
use fastbrowse_core::page::Action;
use serde_json::json;

#[tokio::test(flavor = "multi_thread")]
async fn test_a_pointer_drag_releases_on_an_exposed_point_when_a_cover_hides_the_destination_centre() {
    let Some(h) = harness().await else { return };
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
    assert_eq!(drops, json!(["Done"]), "release must act only on the chosen control: {result:?}");
    assert_eq!(result.outcome, StepOutcome::Executed, "{result:?}");
}
