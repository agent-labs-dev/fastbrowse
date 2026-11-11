mod common;

use common::{eval_active, find, harness, turn};
use fastbrowse_core::models::{Operation, StepOutcome};
use fastbrowse_core::page::Action;

#[tokio::test(flavor = "multi_thread")]
async fn test_a_decorative_animation_does_not_report_an_unchanged_click_as_changed() {
    let _turn = turn().await;
    let Some(h) = harness().await else { panic!("Chrome is required for this regression test") };
    eval_active(
        &h.session,
        "document.body.innerHTML = '<button>Idle</button><div hidden></div>'; \
         document.querySelector('button').onclick = () => { \
           const tick = () => { document.querySelector('div').dataset.tick = performance.now(); }; \
           tick(); window.animation = setInterval(tick, 20); };",
    )
    .await;
    let before = h.observe().await;
    let fingerprint = h.page.fingerprint().await.expect("fingerprint before");
    let action = Action { target_id: Some(find(&before, "Idle").id.clone()), ..Action::new(Operation::Click) };
    let result = h.page.act(&action, &before).await.expect("act");
    eval_active(&h.session, "clearInterval(window.animation)").await;
    let after = h.page.fingerprint().await.expect("fingerprint");
    h.close().await;
    assert_eq!(result.outcome, StepOutcome::Executed, "{result:?}");
    assert_eq!(after, fingerprint, "only a hidden attribute changed");
    assert!(!result.page_changed, "decorative mutations must not count as a page change: {result:?}");
}
