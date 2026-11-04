use fastbrowse_core::citations::{page_text, table_blocks};
use fastbrowse_core::page::Capture;

#[test]
fn downloaded_csv_keeps_literal_pipes_in_its_quote() {
    // BrowserPage::capture appends downloaded CSV verbatim as a Table block.
    let text = "name,expression\nfilter,a | b";
    let capture: Capture = serde_json::from_value(serde_json::json!({
        "url": "https://example.test/report",
        "title": "Report",
        "captured_at": "2026-01-01T00:00:00Z",
        "sha256": "csv-capture",
        "text": text,
        "blocks": [{
            "source_id": "download/0",
            "source_url": "https://example.test/report.csv",
            "complete_source": true,
            "kind": "table",
            "frame_id": null,
            "start": 0,
            "end": text.chars().count(),
            "heading_path": ["Downloaded file: report.csv"]
        }]
    }))
    .unwrap();
    assert_eq!(page_text(text, 0, &table_blocks(&capture)), text);
}
