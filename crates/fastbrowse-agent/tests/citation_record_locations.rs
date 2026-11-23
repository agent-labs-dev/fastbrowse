//! Ordinary document blocks must retain their individual record citations across reads.

use fastbrowse_agent::memory::{Fact, Notes, fact_id};
use fastbrowse_agent::retrieval::{Claim, assemble_answer};
use fastbrowse_core::models::{Evidence, FactReader};
use serde_json::json;

#[test]
fn rereading_a_plain_document_record_preserves_its_identical_sibling() {
    let read = |record: &str, block: usize, start: i64, capture: &str, at: &str| {
        // capture.js uses an empty source_path for ordinary main-document blocks;
        // BrowserPage appends the block index when it constructs their source_id.
        let evidence: Evidence = serde_json::from_value(json!({
            "source_id": format!("main/:{block}"),
            "url": "https://example.test/reports",
            "frame_id": null,
            "captured_at": at,
            "capture_sha256": capture,
            "start": start,
            "end": start + 9,
            "quote": "Available",
            "heading_path": ["Reports"],
            "links": []
        }))
        .unwrap();
        Fact::read(&format!("Report {record} is available"), evidence, FactReader::JevChoice)
    };
    let first = read("A", 0, 10, "before", "2026-11-21T20:29:37Z");
    let second = read("B", 1, 30, "before", "2026-11-21T20:29:37Z");
    let second_again = read("B", 1, 30, "after", "2026-11-21T20:30:12Z");
    let ids = vec![fact_id(&first), fact_id(&second), fact_id(&second_again)];
    let notes = Notes::new([first, second, second_again]);
    let answer = assemble_answer(&[Claim::new("Both reports are available", ids)], &notes, &[]);

    assert!(
        answer.citations.iter().any(|citation| citation.text == "Report A is available"),
        "rereading report B must preserve report A's separate supporting citation"
    );
    assert!(answer.citations.iter().any(|citation| citation.text == "Report B is available"));
}
