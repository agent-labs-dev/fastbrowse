//! A heading or link can state a numeric field rather than only naming a record.

use std::sync::Mutex;

use fastbrowse_agent::memory::{Fact, Notes};
use fastbrowse_agent::retrieval::{compose, page_quotes};
use fastbrowse_core::BoxFuture;
use fastbrowse_core::decision::{DecisionClient, DecisionError, DecisionProfile, Evaluation, Question};
use fastbrowse_core::llm::{Generation, LLMClient, LLMError, Message, ResponseSchema};
use fastbrowse_core::models::{CostBasis, CostComponent, CostLine, FactReader, LLMPurpose};
use fastbrowse_core::page::Capture;
use fastbrowse_core::telemetry::Ledger;
use indexmap::IndexMap;
use serde_json::{Value, json};

#[derive(Default)]
struct Questions(Mutex<Vec<IndexMap<String, Question>>>);

impl DecisionClient for Questions {
    fn evaluate<'a>(
        &'a self,
        _: &'a Value,
        questions: &'a IndexMap<String, Question>,
    ) -> BoxFuture<'a, Result<Evaluation, DecisionError>> {
        self.0.lock().unwrap().push(questions.clone());
        Box::pin(async { Err(DecisionError::failed("Inspect the field request without a paid model")) })
    }

    fn profile(&self) -> DecisionProfile {
        DecisionProfile::jev()
    }
}

struct Minimum;

impl LLMClient for Minimum {
    fn generate_value<'a>(
        &'a self,
        _: LLMPurpose,
        _: &'a [Message],
        schema: &'a dyn ResponseSchema,
        _: Option<&'a Ledger>,
    ) -> BoxFuture<'a, Result<Generation<Value>, LLMError>> {
        Box::pin(async move {
            let draft = json!({
                "claims": [{
                    "text": [{"computed": true}], "evidence_ids": ["e1"],
                    "tally": {"op": "min", "of": "ticket", "field": "price", "written": "$10"}
                }], "columns": null, "numbered": false
            });
            Ok(Generation {
                data: schema.validate_json(&draft.to_string()).map_err(LLMError::schema)?,
                cost: CostLine::new(CostComponent::Llm, CostBasis::Estimated, Some(0.0)),
            })
        })
    }
}

async fn check_numeric_fragment(kind: &str) {
    let text = "Adult ticket price $10";
    let capture: Capture = serde_json::from_value(json!({
        "url": "https://example.test/tickets", "title": "Tickets",
        "captured_at": "2026-11-22T00:00:00Z", "sha256": "capture", "text": text,
        "blocks": [{"source_id": "s0", "kind": kind, "frame_id": null,
                    "start": 0, "end": text.chars().count()}]
    }))
    .unwrap();
    let mut notes = Notes::new(
        page_quotes(&capture).into_iter().map(|quote| Fact::read(&quote.quote.clone(), quote, FactReader::JevChoice)),
    );
    notes.remember_capture(&capture, 1);
    let jev = Questions::default();
    let _ = compose(&jev, &Minimum, "Find the minimum ticket price", &notes, None, &[]).await;
    assert!(
        jev.0.lock().unwrap().iter().flat_map(|batch| batch.values()).any(|question| {
            matches!(question, Question::Choice(q)
            if q.instructions.contains("Does this note give exactly one whole")
                && q.instructions.contains(text)
                && q.criteria.values().any(|value| value.as_str().is_some_and(|s| s.contains("$10"))))
        }),
        "The {kind}'s explicit price must reach Jev's field question instead of being discarded as a name"
    );
}

#[tokio::test]
async fn a_heading_can_state_a_tickets_price() {
    check_numeric_fragment("heading").await;
}

#[tokio::test]
async fn a_link_can_state_a_tickets_price() {
    check_numeric_fragment("link").await;
}
