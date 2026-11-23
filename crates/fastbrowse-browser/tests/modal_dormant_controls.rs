mod common;

use common::{eval_active, harness, sites};

#[tokio::test(flavor = "multi_thread")]
async fn test_a_rendered_aria_hidden_wrapper_with_dormant_controls_is_not_modal() {
    let Some(h) = harness().await else { return };
    h.page.navigate(&sites().main, None).await.expect("navigate");
    // Responsive navigation can hide its inner panel while retaining an aria-hidden outer wrapper.
    eval_active(
        &h.session,
        r#"
        document.body.innerHTML = '<aside aria-hidden="true"><div style="display:none"><button>Menu item</button></div></aside>' +
          '<main><button>Continue</button></main>';
        "#,
    )
    .await;
    let observation = h.observe().await;
    assert_eq!(observation.controls.iter().map(|c| c.label.as_str()).collect::<Vec<_>>(), ["Continue"]);
    assert_eq!(observation.modal, None, "a dormant menu does not put the main page in a modal");
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_display_contents_app_root_still_puts_drawn_controls_behind_a_modal() {
    let Some(h) = harness().await else { return };
    h.page.navigate(&sites().main, None).await.expect("navigate");
    // A layout wrapper with no box does not stop its descendants from being drawn.
    eval_active(
        &h.session,
        r#"
        document.body.innerHTML = '<div aria-hidden="true" style="display:contents"><button>Edit</button></div>' +
          '<div class="portal"><button>Cancel</button><button>OK</button></div>';
        "#,
    )
    .await;
    let observation = h.observe().await;
    assert_eq!(observation.controls.iter().map(|c| c.label.as_str()).collect::<Vec<_>>(), ["Cancel", "OK"]);
    assert_eq!(
        observation.modal.as_deref(),
        Some(""),
        "the app controls are drawn despite their wrapper lacking a box"
    );
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_transparent_aria_hidden_menu_with_dormant_controls_is_not_modal() {
    let Some(h) = harness().await else { return };
    h.page.navigate(&sites().main, None).await.expect("navigate");
    // A menu kept in the page while closed is faded out and inert to the pointer, yet still has a box.
    eval_active(
        &h.session,
        r#"
        document.body.innerHTML = '<aside aria-hidden="true" style="opacity:0;pointer-events:none"><button>Menu item</button></aside>' +
          '<main><button>Continue</button></main>';
        "#,
    )
    .await;
    let observation = h.observe().await;
    assert_eq!(observation.controls.iter().map(|c| c.label.as_str()).collect::<Vec<_>>(), ["Continue"]);
    assert_eq!(observation.modal, None, "a transparent menu does not put the main page in a modal");
    h.close().await;
}
