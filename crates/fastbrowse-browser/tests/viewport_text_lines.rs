//! The visible text the driver reads is laid out as the page shows it, so a quote of it is a quote of the capture.

mod common;

use common::{eval_active, harness, turn};

#[tokio::test(flavor = "multi_thread")]
async fn text_nodes_of_one_line_read_as_one_line() {
    let _turn = turn().await;
    let Some(h) = harness().await else { return };
    // React renders `Total: ${total}` as two text nodes, so saucedemo's total read "Total: $" and "43.18" on two
    // lines, and every quote of it was refused against the capture's "Total: $43.18".
    eval_active(
        &h.session,
        "document.body.innerHTML = '<div id=total></div><p>Sold by <b>Ada</b> <i>Lovelace</i></p>\
         <p>First<br>Second</p><p>Add to cart <button></button> Details</p><div>Outer<div>Inner</div>After</div>';\
         const total = document.querySelector('#total');\
         total.append(document.createTextNode('Total: $'), document.createTextNode('43.18'))",
    )
    .await;
    let lines: Vec<String> = h.observe().await.viewport_text.lines().map(str::to_string).collect();
    for line in
        ["Total: $43.18", "Sold by Ada Lovelace", "Add to cart Details", "First", "Second", "Outer", "Inner", "After"]
    {
        assert!(lines.iter().any(|shown| shown == line), "no line {line:?} in {lines:?}");
    }
    let capture = h.page.capture().await.expect("capture");
    assert!(capture.text.contains("Total: $43.18"), "{:?}", capture.text);
    h.close().await;
}
