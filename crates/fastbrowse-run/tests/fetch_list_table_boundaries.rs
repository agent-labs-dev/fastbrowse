//! A table cell's nested list has its own places, not the enclosing item's place.

use fastbrowse_run::fetch::html::blocks;

#[test]
fn a_list_in_a_table_cell_keeps_each_items_place() {
    let (_, found) = blocks(
        "<ul><li>First outer</li><li><table><tr><td><ul>\
         <li>Alpha</li><li>Beta</li><li>Gamma</li>\
         </ul></td></tr></table></li></ul>",
        "https://example.test/",
    );
    let mut missing = Vec::new();
    for (text, position) in [("Alpha", 1), ("Beta", 2), ("Gamma", 3)] {
        if !found.iter().any(|block| {
            block.text.contains(text)
                && block.item_place.is_some_and(|place| place.position == position && place.count == 3)
                && ["Alpha", "Beta", "Gamma"].iter().filter(|name| block.text.contains(**name)).count() == 1
        }) {
            missing.push((text, position));
        }
    }
    assert!(missing.is_empty(), "items without their own place: {missing:?}");
}
