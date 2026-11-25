//! Selecting citations for a large collected list must not rescan every block for unrelated quotes.

use std::time::{Duration, Instant};

use fastbrowse_agent::memory::{Fact, Notes, fact_id};
use fastbrowse_agent::retrieval::{Claim, assemble_answer};
use fastbrowse_core::models::{Evidence, FactReader};
use fastbrowse_core::page::Capture;
use fastbrowse_core::py::sha256_hex;
use serde_json::json;

#[test]
fn unrelated_quotes_in_large_captures_are_cited_quickly() {
    let mut notes = Notes::default();
    let mut ids = Vec::new();
    for page in 0..2 {
        let mut text = String::new();
        let mut blocks = Vec::new();
        for index in 0..900 {
            let start = text.len();
            text.push_str(&format!("Report {} is available", page * 900 + index));
            blocks.push(json!({
                "source_id": format!("main/:{index}"), "kind": "list_item", "frame_id": null,
                "start": start, "end": text.len(), "whole_record": true,
            }));
            text.push_str("\n\n");
        }
        let capture: Capture = serde_json::from_value(json!({
            "url": "https://example.test/reports", "title": "Reports",
            "captured_at": format!("2026-11-25T00:00:0{page}Z"),
            "sha256": sha256_hex(text.as_bytes()), "text": text, "blocks": blocks,
            "inaccessible_frames": 0,
        }))
        .unwrap();
        notes.remember_capture(&capture, 0);
        for block in &capture.blocks {
            let quote = capture.block_text(block);
            let evidence: Evidence = serde_json::from_value(json!({
                "source_id": block.source_id, "url": capture.url, "frame_id": null,
                "captured_at": capture.captured_at, "capture_sha256": capture.sha256,
                "start": block.start, "end": block.end, "quote": quote,
            }))
            .unwrap();
            let fact = Fact::read(quote, evidence, FactReader::JevChoice);
            ids.push(fact_id(&fact));
            notes.add(fact);
        }
    }
    let started = Instant::now();
    let answer = assemble_answer(&[Claim::new("All reports are available", ids)], &notes, &[]);
    let elapsed = started.elapsed();
    assert_eq!(answer.citations.len(), 1800);
    assert!(elapsed < Duration::from_secs(5), "selecting citations took {elapsed:?}");
}
