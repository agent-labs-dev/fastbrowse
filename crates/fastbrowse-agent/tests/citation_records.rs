//! Reading one record again must preserve citations for its siblings.

use fastbrowse_agent::memory::{Fact, Notes, fact_id};
use fastbrowse_agent::retrieval::{Claim, assemble_answer};
use fastbrowse_core::models::{Evidence, FactReader};
use serde_json::json;

#[test]
fn rereading_one_of_two_records_preserves_the_other_records_citation() {
    let read = |record: &str, start: i64, capture: &str, at: &str| {
        let evidence: Evidence = serde_json::from_value(json!({
            "source_id": record,
            "url": "https://example.test/reports",
            "frame_id": null,
            "captured_at": at,
            "capture_sha256": capture,
            "start": start,
            "end": start + 8,
            "quote": "Download",
            "heading_path": ["Reports"],
            "links": [{
                "text": "Download", "start": start, "end": start + 8,
                "href": format!("https://example.test/{record}.pdf")
            }]
        }))
        .unwrap();
        Fact::read(&format!("Report {record}"), evidence, FactReader::JevChoice)
    };
    let first = read("report-a", 10, "before", "2026-11-21T20:29:37Z");
    let second = read("report-b", 30, "before", "2026-11-21T20:29:37Z");
    let second_again = read("report-b", 30, "after", "2026-11-21T20:30:12Z");
    let ids = vec![fact_id(&first), fact_id(&second), fact_id(&second_again)];
    let notes = Notes::new([first, second, second_again]);
    let answer = assemble_answer(&[Claim::new("There are two reports", ids)], &notes, &[]);

    assert!(
        answer.citations.iter().any(|citation| citation.text == "Report report-a"),
        "a newer read of report-b must not erase report-a's support"
    );
    assert!(answer.citations.iter().any(|citation| citation.text == "Report report-b"));
}
