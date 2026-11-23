mod common;

use common::{eval_active, harness, sites};

#[tokio::test(flavor = "multi_thread")]
async fn test_an_unmarked_modal_keeps_controls_in_open_shadow_roots_inside_it() {
    let Some(h) = harness().await else { return };
    h.page.navigate(&sites().main, None).await.expect("navigate");
    // A modal's custom element controls are captured through their open shadow roots.
    eval_active(
        &h.session,
        r#"
        document.body.innerHTML = '<div aria-hidden="true"><button>Edit</button></div>' +
          '<div class="portal"><button>Cancel</button><div id="confirm-control"></div></div>';
        document.querySelector('#confirm-control').attachShadow({mode:'open'}).innerHTML = '<button>OK</button>';
        "#,
    )
    .await;
    let observation = h.observe().await;
    assert_eq!(observation.controls.iter().map(|c| c.label.as_str()).collect::<Vec<_>>(), ["Cancel", "OK"]);
    assert_eq!(observation.modal.as_deref(), Some(""), "the captured shadow control belongs to the modal");
    h.close().await;
}
