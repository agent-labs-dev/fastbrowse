use fastbrowse_core::citations::{page_text, table_blocks};
use fastbrowse_core::page::Capture;

#[test]
fn fetched_table_preserves_a_dash_data_row_after_an_ordinary_data_row() {
    // fetch::html::emit_table emits the header, its divider, and each data row separately.
    let rows = ["| Name |", "| --- |", "| Ada |", "| --- |"];
    let mut text = String::new();
    let mut blocks = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        let start = text.chars().count();
        text.push_str(row);
        blocks.push(serde_json::json!({
            "source_id": format!("fetch/:{index}"),
            "kind": "table", "frame_id": null,
            "start": start, "end": text.chars().count()
        }));
        text.push_str("\n\n");
    }
    let capture: Capture = serde_json::from_value(serde_json::json!({
        "url": "https://example.test/scores", "title": "Scores",
        "captured_at": "2026-01-01T00:00:00Z", "sha256": "fetched-capture",
        "text": text, "blocks": blocks
    }))
    .unwrap();
    let row = &capture.blocks[3];
    assert_eq!(page_text(rows[3], row.start, &table_blocks(&capture)), "---");
}
