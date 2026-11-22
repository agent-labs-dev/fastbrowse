//! A path inside a surface's rectangle can still start on a different control.

mod common;

use common::{eval_active, find, harness};
use fastbrowse_core::models::{Operation, StepOutcome};
use fastbrowse_core::page::{Action, DragPath, DragPoint};
use serde_json::json;

#[tokio::test(flavor = "multi_thread")]
async fn test_a_drawn_drag_does_not_press_a_control_covering_its_start_point() {
    let h = harness().await.expect("Chrome is required for this regression test");
    h.goto("/canvas-drag.html").await;
    // The canvas centre remains exposed, while a toolbar button covers the proposed stroke's start.
    eval_active(&h.session, "window.toolbarPresses = 0; document.body.insertAdjacentHTML('beforeend', '<button style=\"position:fixed;left:40px;top:70px;width:100px;height:80px;z-index:5\" onpointerdown=\"window.toolbarPresses++\">Clear drawing</button>')").await;
    let observation = h.observe().await;
    let target = find(&observation, "Drawing pad");
    let action = Action {
        target_id: Some(target.id.clone()),
        drag_path: Some(DragPath {
            press: DragPoint { x: 0.2, y: 0.5 },
            points: vec![DragPoint { x: 0.6, y: 0.5 }],
            release: DragPoint { x: 0.8, y: 0.5 },
            shift: false,
        }),
        ..Action::new(Operation::Drag)
    };
    let result = h.page.act(&action, &observation).await.unwrap();
    assert_eq!(
        eval_active(&h.session, "window.toolbarPresses").await,
        json!(0),
        "a drag on Drawing pad pressed its Clear drawing toolbar button: {result:?}"
    );
    assert_ne!(result.outcome, StepOutcome::Executed, "the covered start must be refused");
    h.close().await;
}
