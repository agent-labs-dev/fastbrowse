//! Every point of a drawn gesture must keep targeting its selected surface unless it captures the pointer.

mod common;

use common::{eval_active, find, harness};
use fastbrowse_core::models::{Operation, StepOutcome};
use fastbrowse_core::page::{Action, DragPath, DragPoint};
use serde_json::json;

async fn draw_across_toolbar(capture: bool, covered_release: bool) {
    let h = harness().await.expect("Chrome is required for this regression test");
    h.goto("/canvas-drag.html").await;
    eval_active(&h.session, &format!(r#"
        window.toolbarEvents = [];
        document.body.insertAdjacentHTML('beforeend', '<button id="toolbar" style="position:fixed;left:240px;top:70px;width:100px;height:80px;z-index:5">Clear drawing</button>');
        document.getElementById('toolbar').onpointermove = event => {{ if (event.buttons) window.toolbarEvents.push('move'); }};
        document.getElementById('toolbar').onpointerup = () => window.toolbarEvents.push('release');
        if (!{capture}) pad.setPointerCapture = () => {{}};
    "#)).await;
    let observation = h.observe().await;
    let action = Action {
        target_id: Some(find(&observation, "Drawing pad").id.clone()),
        drag_path: Some(DragPath {
            press: DragPoint { x: 0.2, y: 0.5 },
            points: vec![DragPoint { x: if covered_release { 0.3 } else { 0.7 }, y: 0.5 }],
            release: DragPoint { x: if covered_release { 0.7 } else { 0.9 }, y: 0.5 },
            shift: false,
        }),
        ..Action::new(Operation::Drag)
    };
    let result = h.page.act(&action, &observation).await.unwrap();
    assert_eq!(
        eval_active(&h.session, "window.toolbarEvents").await,
        json!([]),
        "the drawn drag dispatched held-button input to another control: {result:?}"
    );
    if capture {
        assert_eq!(result.outcome, StepOutcome::Executed, "pointer capture must allow travel over the toolbar");
    } else {
        assert_ne!(
            result.outcome,
            StepOutcome::Executed,
            "an uncaptured path crossing another control must be refused"
        );
    }
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_drawn_drag_checks_movement_destinations() {
    draw_across_toolbar(false, false).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_drawn_drag_checks_release_destination() {
    draw_across_toolbar(false, true).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_drawn_drag_allows_pointer_capture_over_a_toolbar() {
    draw_across_toolbar(true, false).await;
}
