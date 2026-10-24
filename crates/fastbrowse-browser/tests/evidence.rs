mod common;
mod misc_support;

use std::collections::HashSet;

use common::{harness, sites};
use fastbrowse_core::page::BlockKind;
use misc_support::{eval_active, text_of};

// -- test_comment_context.py: reply controls belong to their comment record, including when only one form is open --

#[tokio::test(flavor = "multi_thread")]
async fn test_one_open_reply_form_does_not_inherit_the_first_commenter() {
    for record in ["li", "article", r#"div role="listitem""#] {
        let Some(h) = harness().await else { return };
        let tag = record.split(' ').next().unwrap();
        h.page
            .navigate(
                &format!(
                    "data:text/html,<form><textarea aria-label='Comment text area'></textarea><button>Post</button></form>\
                     <section><h2>Comments</h2><ul>\
                     <{record}><a>First commenter</a><p>First request</p><button>Reply</button></{tag}>\
                     <{record}><a>Merlinqt</a><p>Scale map icons</p><button>Reply</button>\
                     <form><textarea aria-label='Comment text area'>@Merlinqt </textarea>\
                     <p>Please follow our Terms of Service</p><button>Cancel</button><button>Post</button></form></{tag}>\
                     </ul></section>"
                ),
                None,
            )
            .await
            .expect("navigate");
        let observation = h.observe().await;
        let field = observation
            .controls
            .iter()
            .find(|c| c.label == "Comment text area" && c.value.as_deref() == Some("@Merlinqt "))
            .unwrap();
        let posts: Vec<_> = observation.controls.iter().filter(|c| c.label == "Post").collect();
        assert_eq!(field.context.as_deref(), Some("Merlinqt"), "{record}");
        assert_eq!(
            posts.iter().map(|c| c.context.as_deref()).collect::<Vec<_>>(),
            [None, Some("Merlinqt")],
            "{record}"
        );
        h.close().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_list_records_whether_its_element_holds_a_frame() {
    for inside in [false, true] {
        let Some(h) = harness().await else { return };
        let frame = "<iframe sandbox srcdoc='<p>Advertisement</p>'></iframe>";
        let url = format!(
            "data:text/html,<h1>Birch</h1>{}<h2>Comments (2)</h2><ul><li><p>Elm: The map crashes.</p></li>\
             <li><p>Oak: Thanks.</p>{}</li></ul>",
            if inside { "" } else { frame },
            if inside { frame } else { "" }
        );
        h.page.navigate(&url, None).await.expect("navigate");
        let captured = h.page.capture().await.expect("capture");
        let members: Vec<_> = captured.blocks.iter().filter(|b| b.list_id.is_some()).collect();
        assert!(!members.is_empty(), "inside={inside}");
        assert_eq!(members.iter().map(|b| b.list_frameless).collect::<HashSet<_>>(), HashSet::from([!inside]));
        h.close().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_list_that_hosts_a_shadow_root_is_not_frameless() {
    let Some(h) = harness().await else { return };
    h.page
        .navigate(
            "data:text/html,<h1>Birch</h1><h2>Comments (2)</h2>\
             <div id=list role=list><div role=listitem><p>Elm: The map crashes.</p></div>\
             <div role=listitem><p>Oak: Thanks.</p></div></div>\
             <script>document.getElementById('list').attachShadow({mode: 'open'}).innerHTML = \
             \"<slot></slot><iframe sandbox srcdoc='<p>More</p>'></iframe>\";</script>",
            None,
        )
        .await
        .expect("navigate");
    let captured = h.page.capture().await.expect("capture");
    let members: Vec<_> = captured.blocks.iter().filter(|b| b.list_id.is_some()).collect();
    assert!(!members.is_empty());
    assert_eq!(members.iter().map(|b| b.list_frameless).collect::<HashSet<_>>(), HashSet::from([false]));
    h.close().await;
}

// -- test_image_evidence.py: rendered image elements need DOM evidence even when they contain no text -----------

#[tokio::test(flavor = "multi_thread")]
async fn test_a_figure_is_visual_content_but_an_icon_sized_drawing_is_not() {
    let Some(h) = harness().await else { return };
    h.page.navigate(&sites().main, None).await.expect("navigate");
    eval_active(
        &h.session,
        r#"
        document.body.innerHTML = '<h1>Report</h1><p>' + 'word '.repeat(40) + '</p>' +
          '<canvas width="320" height="240"></canvas>';
        "#,
    )
    .await;
    assert!(h.observe().await.visual_content);
    eval_active(
        &h.session,
        r#"
        document.body.innerHTML = '<h1>Report</h1><p>' + 'word '.repeat(40) + '</p>' +
          '<canvas width="16" height="16"></canvas>';
        "#,
    )
    .await;
    assert!(!h.observe().await.visual_content);
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_drawing_below_the_fold_does_not_ask_for_a_screenshot() {
    let Some(h) = harness().await else { return };
    h.page.navigate(&sites().main, None).await.expect("navigate");
    eval_active(
        &h.session,
        r#"
        document.body.innerHTML = '<h1>Report</h1><p>' + 'word '.repeat(40) + '</p>' +
          '<div style="height:4000px"></div><canvas width="320" height="240"></canvas>';
        "#,
    )
    .await;
    assert!(!h.observe().await.visual_content);
    // Scrolled into view, the same drawing is page state the screenshot does carry.
    eval_active(&h.session, "scrollTo(0, document.body.scrollHeight)").await;
    assert!(h.observe().await.visual_content);
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_capture_reports_visible_image_metadata_without_inventing_pixels() {
    let Some(h) = harness().await else { return };
    h.page.navigate(&sites().main, None).await.expect("navigate");
    eval_active(
        &h.session,
        r#"
        document.body.innerHTML = '<h1>README</h1><a href="/map.png">' +
          '<img src="/map.png" alt="Minimap screenshot"></a>' +
          '<img src="/hidden.png" alt="Private image" style="display:none">';
    "#,
    )
    .await;
    let capture = h.page.capture().await.expect("capture");
    assert!(capture.text.contains("Minimap screenshot"));
    assert!(capture.text.contains("/map.png"));
    assert!(!capture.text.contains("Private image"));
    assert!(capture.text.contains("Image element"));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_capture_does_not_copy_inline_image_payload() {
    let Some(h) = harness().await else { return };
    h.page.navigate(&sites().main, None).await.expect("navigate");
    eval_active(
        &h.session,
        r#"
        document.body.innerHTML = '<img alt="Inline chart">';
        document.querySelector('img').src = 'data:image/png;base64,' + 'PAYLOAD'.repeat(100000);
        "#,
    )
    .await;
    let capture = h.page.capture().await.expect("capture");
    assert!(capture.text.contains("Inline chart"));
    assert!(capture.text.contains("data:image/png;base64,[payload omitted]"));
    assert!(!capture.text.contains("PAYLOAD"));
    assert!(capture.text.chars().count() < 2000);
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_image_observations_are_bounded_and_ignore_decode_state() {
    let Some(h) = harness().await else { return };
    h.page.navigate(&sites().main, None).await.expect("navigate");
    eval_active(
        &h.session,
        r#"
        document.body.innerHTML = Array.from({length: 300}, (_, i) =>
          `<img alt="Chart ${i}" src="blob:${'x'.repeat(4000)}">`).join('');
        "#,
    )
    .await;
    let before = h.page.capture().await.expect("capture");
    assert_eq!(before.text.matches("Image element DOM metadata").count(), 32);
    assert!(before.text.contains("blob:[identifier omitted]"));
    eval_active(
        &h.session,
        r#"
        for (const image of document.querySelectorAll('img')) {
          Object.defineProperty(image, 'complete', {value: true});
          Object.defineProperty(image, 'naturalWidth', {value: 100});
        }
        "#,
    )
    .await;
    assert_eq!(h.page.capture().await.expect("capture").text, before.text);
    h.close().await;
}

// -- test_link_evidence.py: a link drawn only as an icon says where it goes and nothing else, so its destination
// must reach the capture ----------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn test_capture_reports_icon_link_destination_with_its_row() {
    let Some(h) = harness().await else { return };
    h.page.navigate(&sites().main, None).await.expect("navigate");
    eval_active(
        &h.session,
        r##"
        document.body.innerHTML = '<h1>Releases</h1><div class="row"><span>v2.1.0</span> <span>12 KB</span> ' +
          '<span><a href="/files/abc.zip?sig=1"><svg viewBox="0 0 8 8"><path d="M0 0h8v8z"></path></svg></a>' +
          '</span></div><a href="/about">About</a>' +
          '<a href="/private.zip" style="display:none"><svg></svg></a><a href="#menu"><svg></svg></a>' +
          '<a href="#"><svg></svg></a><a href="http://["><svg></svg></a>';
    "##,
    )
    .await;
    let capture = h.page.capture().await.expect("capture");
    let destination = format!("{}/files/abc.zip?sig=1", sites().main.trim_end_matches('/'));
    let observed: Vec<String> = capture
        .blocks
        .iter()
        .filter(|b| b.kind == BlockKind::Observation)
        .map(|b| text_of(&capture, b))
        .filter(|t| t.contains(&destination))
        .collect();
    assert_eq!(observed.len(), 1);
    assert!(observed[0].contains("v2.1.0 12 KB"));
    // A link with words says what it is in the text already; a hidden one is not on the page a user sees.
    assert!(!capture.text.contains("/about"));
    assert!(!capture.text.contains("/private.zip"));
    assert!(!capture.text.contains("#menu"));
    // A bare `#` is a placeholder whose parsed hash is empty, and an unparsable address must not end the capture.
    assert_eq!(capture.text.matches("Link element DOM metadata").count(), 1);
    assert!(!capture.text.contains("http://["));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_icon_link_observations_are_bounded() {
    let Some(h) = harness().await else { return };
    h.page.navigate(&sites().main, None).await.expect("navigate");
    eval_active(
        &h.session,
        r#"
        document.body.innerHTML = Array.from({length: 100}, (_, i) =>
          `<p>Row ${i} <a href="/f/${i}?k=${'x'.repeat(4000)}"><svg></svg></a></p>`).join('');
        "#,
    )
    .await;
    let capture = h.page.capture().await.expect("capture");
    assert_eq!(capture.text.matches("Link element DOM metadata").count(), 32);
    assert!(capture.text.contains("Additional link destinations omitted."));
    assert!(capture.text.chars().count() < 100_000);
    h.close().await;
}
