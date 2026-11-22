//! Result evidence must retain the separate records preserved by answer assembly.

use super::*;
use crate::memory::fact_id;
use crate::retrieval::{Claim, assemble_answer};

#[test]
fn selecting_a_siblings_latest_read_preserves_both_records_in_result_evidence() {
    let read = |record: &str, block: usize, start: i64, capture: &str, at: &str| {
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
    let ids = vec![fact_id(&first), fact_id(&second_again)];
    let notes = Notes::new([first, second, second_again]);
    let answer = assemble_answer(&[Claim::new("Both reports are available", ids)], &notes, &[]);
    assert_eq!(answer.citations.len(), 2, "answer assembly preserves both records");

    let ledger = Arc::new(Ledger::new(Limits::default()));
    let mut state = RunState::new(
        "Report availability".into(),
        IndexMap::new(),
        Vec::new(),
        Authorization::default(),
        ledger.clone(),
    );
    state.notes = notes.clone();
    let result = agent().answered(&answer, &notes, Some(&state), &ledger, Status::Complete, None, None);

    assert_eq!(result.citations.len(), 2);
    assert!(
        result.evidence.iter().any(|e| e.capture_sha256 == "before" && e.start == 10),
        "result evidence must retain record A, whose sibling's older read is still in the run's notes"
    );
    assert!(result.evidence.iter().any(|e| e.capture_sha256 == "after" && e.start == 30));
}
