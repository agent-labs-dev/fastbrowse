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
