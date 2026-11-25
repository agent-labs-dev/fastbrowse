//! List positions must not turn a fragment of a card into whole-record evidence.

use fastbrowse_run::fetch::html::blocks;

#[test]
fn a_card_containing_items_does_not_mark_its_tail_as_the_whole_record() {
    let mut partial = Vec::new();
    for (kind, variants) in [
        ("aria items", "<div role='list'><span role='listitem'>Red</span><span role='listitem'>Blue</span></div>"),
        (
            "declared places",
            "<div role='tablist'><span role='tab' aria-posinset='1' aria-setsize='2'>Red</span>\
             <span role='tab' aria-posinset='2' aria-setsize='2'>Blue</span></div>",
        ),
        ("native items with blocks", "<ul><li><p>Red</p></li><li><p>Blue</p></li></ul>"),
    ] {
        let card = |name, price| {
            format!("<div class='card'><h2>{name}</h2><p>Price ${price}</p>{variants}<p>Available</p></div>")
        };
        let html = format!("<main><h1>Products</h1>{}{}</main>", card("Product A", 10), card("Product B", 20));
        let (_, found) = blocks(&html, "https://example.test/");
        for block in found.iter().filter(|block| block.whole_record) {
            let complete = [
                ["Product A", "Price $10", "Red", "Blue", "Available"],
                ["Product B", "Price $20", "Red", "Blue", "Available"],
            ]
            .iter()
            .any(|parts| parts.iter().all(|part| block.text.contains(part)));
            if !complete {
                partial.push((kind, block.text.clone()));
            }
        }
    }
    assert!(partial.is_empty(), "fragments marked as whole cards: {partial:?}");
}
