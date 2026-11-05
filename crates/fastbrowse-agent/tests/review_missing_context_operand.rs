mod common;

use common::block_on;
use fastbrowse_agent::memory::Notes;
use fastbrowse_agent::planner::{Requirement, RequirementKind};
use fastbrowse_agent::retrieval::{ReadOptions, read};
use fastbrowse_core::BoxFuture;
use fastbrowse_core::llm::{Generation, LLMClient, LLMError, Message, ResponseSchema};
use fastbrowse_core::models::{CostBasis, CostComponent, CostLine, LLMPurpose};
use fastbrowse_core::page::{Block, BlockKind, Capture};
use fastbrowse_core::py::datetime::DateTime;
use fastbrowse_core::py::sha256_hex;
use fastbrowse_core::telemetry::Ledger;
use serde_json::{Value, json};

struct FixedLLM(Value);

impl LLMClient for FixedLLM {
    fn generate_value<'a>(
        &'a self,
        _: LLMPurpose,
        _: &'a [Message],
        _: &'a dyn ResponseSchema,
        _: i64,
        _: Option<&Ledger>,
    ) -> BoxFuture<'a, Result<Generation<Value>, LLMError>> {
        Box::pin(async move {
            Ok(Generation {
                data: self.0.clone(),
                cost: CostLine::new(CostComponent::Llm, CostBasis::Estimated, Some(0.0)),
            })
        })
    }
}

#[test]
fn a_conclusion_cannot_close_a_requirement_after_losing_a_context_operand() {
    let text = "Alpha costs $10.\n\nBeta costs $5.";
    let blocks = [("s0", 0, 16), ("s1", 18, 32)]
        .into_iter()
        .map(|(id, start, end)| Block {
            source_id: id.into(),
            source_url: None,
            complete_source: false,
            kind: BlockKind::Paragraph,
            frame_id: None,
            start,
            end,
            heading_path: Vec::new(),
            control_context: None,
            href: None,
            list_id: None,
            list_count: None,
            list_frameless: false,
        })
        .collect();
    let capture = Capture {
        url: "https://example.test".into(),
        title: "Prices".into(),
        captured_at: DateTime::new(2026, 1, 1, 0, 0, 0).utc(),
        sha256: sha256_hex(text.as_bytes()),
        text: text.into(),
        blocks,
        inaccessible_frames: 0,
    };
    let llm = FixedLLM(json!({
        "claims": [
            {
                "cite": {"first": "s1", "last": "s1"},
                "records": [{"first": "s9", "last": "s9"}],
                "text": "Beta costs $5.",
                "requirement_id": null
            },
            {
                "cite": {"first": "s0", "last": "s0"},
                "records": [{"first": "s0", "last": "s0"}],
                "text": "Alpha costs $10.",
                "requirement_id": null
            },
            {
                "cite": null,
                "draws_on": ["claim:0", "claim:1"],
                "text": "Alpha is cheapest.",
                "requirement_id": "R7"
            }
        ],
        "answered": true
    }));
    let options = ReadOptions {
        requirements: vec![Requirement::new("R7", "Which item is cheapest?", RequirementKind::Information)],
        preserve_collections: true,
        ..ReadOptions::default()
    };
    let mut notes = Notes::default();
    let outcome =
        block_on(read(&llm, &capture, "Which item is cheapest?", &["R7".into()], &mut notes, &options)).expect("read");
    assert!(outcome.rejected_claims >= 1, "the unresolved operand was rejected");
    assert!(
        !notes.evidenced("R7"),
        "a conclusion depending on rejected claim:0 must not evidence R7: {:?}",
        outcome.facts
    );
}
