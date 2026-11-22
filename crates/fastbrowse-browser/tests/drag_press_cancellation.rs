//! A press can reach Chrome before its CDP reply reaches the caller.

mod common;
mod dispatch_support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use common::{chrome, eval_active, find, harness_on, wait_until};
use dispatch_support::unanswering_proxy;
use fastbrowse_core::models::Operation;
use fastbrowse_core::page::{Action, DragPath, DragPoint};

#[tokio::test(flavor = "multi_thread")]
async fn test_cancelled_drawn_drag_releases_an_unanswered_press() {
    let mut connection = chrome().expect("Chrome is required for this regression test").clone();
    let releases = Arc::new(AtomicUsize::new(0));
    let seen = releases.clone();
    connection.cdp_url = unanswering_proxy(connection.cdp_url.clone(), move |command| {
        if command["method"] != "Input.dispatchMouseEvent" {
            return false;
        }
        if command["params"]["type"] == "mouseReleased" {
            seen.fetch_add(1, Ordering::SeqCst);
        }
        command["params"]["type"] == "mousePressed"
    })
    .await;
    let h = harness_on(connection, Default::default(), Default::default()).await.unwrap();
    h.goto("/canvas-drag.html").await;
    eval_active(
        &h.session,
        "window.wasPressed = false; document.addEventListener('pointerdown', () => window.wasPressed = true, true)",
    )
    .await;
    let observation = h.observe().await;
    let action = Action {
        target_id: Some(find(&observation, "Drawing pad").id.clone()),
        drag_path: Some(DragPath {
            press: DragPoint { x: 0.2, y: 0.5 },
            points: vec![DragPoint { x: 0.6, y: 0.5 }],
            release: DragPoint { x: 0.8, y: 0.5 },
            shift: false,
        }),
        ..Action::new(Operation::Drag)
    };
    let mut act = Box::pin(h.page.act(&action, &observation));
    tokio::select! {
        result = &mut act => panic!("the press reply was supposed to be withheld: {result:?}"),
        () = wait_until(|| async { eval_active(&h.session, "window.wasPressed").await == true }, 5.0) => {}
    }
    drop(act);
    // Give the drop guard's asynchronous release time to reach the proxy.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(releases.load(Ordering::SeqCst) > 0, "cancelling a delivered press left the mouse button held");
    h.close().await;
}
