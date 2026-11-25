mod common;

use common::{eval_active, harness, turn};

/// A uniquely named field still needs its enclosing section's accessible name.
#[tokio::test(flavor = "multi_thread")]
async fn uniquely_named_fields_keep_their_aria_named_section_context() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    h.goto("/icons.html").await;
    eval_active(
        &h.session,
        r#"document.body.innerHTML = '<section aria-label="Shipping"><input aria-label="Street address"></section><section aria-label="Billing"><input aria-label="Postal code"></section>'"#,
    )
    .await;
    let observation = h.page.observe().await.expect("observe");
    for (label, section) in [("Street address", "Shipping"), ("Postal code", "Billing")] {
        let field = observation.controls.iter().find(|c| c.label == label).expect("field");
        assert_eq!(field.context.as_deref(), Some(section), "{label} lost its section: {field:?}");
    }
    h.close().await;
}

/// Twin actions in cards named only by aria-label keep their card as context; naming them once aborted the snapshot.
#[tokio::test(flavor = "multi_thread")]
async fn twin_actions_keep_their_aria_named_card_context() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    h.goto("/icons.html").await;
    eval_active(
        &h.session,
        r#"document.body.innerHTML = '<ul><li aria-label="Ada comment"><button>Reply</button></li><li aria-label="Grace comment"><button>Reply</button></li></ul>'"#,
    )
    .await;
    let observation = h.page.observe().await.expect("observe ARIA-named cards");
    let contexts: Vec<_> =
        observation.controls.iter().filter(|c| c.label == "Reply").map(|c| c.context.as_deref()).collect();
    assert_eq!(contexts, vec![Some("Ada comment"), Some("Grace comment")]);
    h.close().await;
}

/// A control with no name of its own reads its enclosing section's accessible name.
#[tokio::test(flavor = "multi_thread")]
async fn unnamed_actions_keep_their_aria_named_section_context() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    h.goto("/icons.html").await;
    eval_active(
        &h.session,
        r#"document.body.innerHTML = '<section aria-label="Gallery"><button style="width:40px;height:40px"></button></section>'"#,
    )
    .await;
    let observation = h.page.observe().await.expect("observe unnamed action");
    let button = observation.controls.iter().find(|c| c.role == "button").expect("button");
    assert_eq!(button.context.as_deref(), Some("Gallery"));
    h.close().await;
}
