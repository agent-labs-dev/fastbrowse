//! Atomic containers must not lend their enclosing item's place to a nested list.

mod common;
mod misc_support;

use common::harness;
use misc_support::{content_capture, set_body, text_of};

#[tokio::test(flavor = "multi_thread")]
async fn nested_lists_in_atomic_containers_keep_each_items_place() {
    let Some(h) = harness().await else { return };
    h.page.navigate(&common::sites().main, None).await.expect("navigate");
    let mut missing = Vec::new();
    for (kind, content) in [
        ("table", "<table><tr><td><ul><li>Alpha</li><li>Beta</li><li>Gamma</li></ul></td></tr></table>"),
        (
            "pre",
            "<pre><span role='list'><span role='listitem'>Alpha</span> \
             <span role='listitem'>Beta</span> <span role='listitem'>Gamma</span></span></pre>",
        ),
    ] {
        set_body(&h.session, &format!("<ul><li>First outer</li><li>{content}</li></ul>")).await;
        let capture = content_capture(&h.page).await;
        for (text, position) in [("Alpha", 1), ("Beta", 2), ("Gamma", 3)] {
            if !capture.blocks.iter().any(|block| {
                let content = text_of(&capture, block);
                content.contains(text)
                    && block.item_place.is_some_and(|place| place.position == position && place.count == 3)
                    && ["Alpha", "Beta", "Gamma"].iter().filter(|name| content.contains(**name)).count() == 1
            }) {
                missing.push((kind, text, position));
            }
        }
    }
    h.close().await;
    assert!(missing.is_empty(), "items without their own place: {missing:?}");
}
