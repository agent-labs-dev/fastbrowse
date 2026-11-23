//! Typed page labels must be explained when they qualify another note's tally membership or field.

use std::sync::Mutex;

use fastbrowse_agent::memory::{Fact, Notes};
use fastbrowse_agent::retrieval::{compose, page_quotes};
use fastbrowse_core::BoxFuture;
use fastbrowse_core::decision::{DecisionClient, DecisionError, DecisionProfile, Evaluation, Question};
use fastbrowse_core::llm::{Generation, LLMClient, LLMError, Message, ResponseSchema};
use fastbrowse_core::models::{CostBasis, CostComponent, CostLine, FactReader, Held, LLMPurpose, SourceControl};
use fastbrowse_core::page::Capture;
use fastbrowse_core::telemetry::Ledger;
use indexmap::IndexMap;
use serde_json::{Value, json};

#[derive(Default)]
struct Questions(Mutex<Vec<(Value, IndexMap<String, Question>)>>);

impl DecisionClient for Questions {
    fn evaluate<'a>(
        &'a self,
        state: &'a Value,
        questions: &'a IndexMap<String, Question>,
    ) -> BoxFuture<'a, Result<Evaluation, DecisionError>> {
        self.0.lock().unwrap().push((state.clone(), questions.clone()));
        Box::pin(async { Err(DecisionError::failed("No paid model is needed to inspect the request")) })
    }

    fn profile(&self) -> DecisionProfile {
        DecisionProfile::jev()
    }
}

struct CountOrSum(&'static str);

impl LLMClient for CountOrSum {
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
                    "text": [{"computed": true}],
                    "evidence_ids": ["e1"],
                    "tally": {
                        "op": self.0, "of": "published product", "written": "1",
                        "field": if self.0 == "sum" { Some("price") } else { None },
                    }
                }],
                "columns": null, "numbered": false,
            });
            Ok(Generation {
                data: schema.validate_json(&draft.to_string()).map_err(LLMError::schema)?,
                cost: CostLine::new(CostComponent::Llm, CostBasis::Estimated, Some(0.0)),
            })
        })
    }
}

async fn check_context(op: &'static str) {
    let mut capture: Capture = serde_json::from_value(json!({
        "url": "https://example.test/products", "title": "Products",
        "captured_at": "2026-11-22T00:00:00Z", "sha256": "capture",
        "text": "Published products\n\nProduct A $10",
        "blocks": [
            {"source_id": "s0", "kind": "heading", "frame_id": null, "start": 0, "end": 18},
            {"source_id": "s1", "kind": "paragraph", "frame_id": null, "start": 20, "end": 33},
        ], "inaccessible_frames": 0,
    }))
    .unwrap();
    capture.blocks[0].control_context = Some(SourceControl {
        role: "editable".into(),
        label: String::new(),
        context: None,
        document: Some("doc".into()),
        held: vec![Held { element: "main:7".into(), value: "Published products".into() }],
    });
    let mut notes = Notes::new(
        page_quotes(&capture).into_iter().map(|quote| Fact::read(&quote.quote.clone(), quote, FactReader::JevChoice)),
    );
    notes.remember_capture(&capture, 1);
    notes.remember_typing(1, "doc", "main:7", "Published products");
    let jev = Questions::default();
    let _ = compose(&jev, &CountOrSum(op), "Count or total the published products", &notes, None, &[]).await;
    let requests = jev.0.lock().unwrap();
    let mut checked = 0;
    for (state, questions) in requests.iter() {
        if !state.to_string().contains("typed_by_step") {
            continue;
        }
        for question in questions.values() {
            let instructions = match question {
                Question::Noul(q) => &q.instructions,
                Question::Choice(q) => &q.instructions,
                _ => continue,
            };
            if !instructions.contains("Product A") {
                continue;
            }
            checked += 1;
            assert!(
                instructions.contains("never that the source stated it on its own")
                    || state.to_string().contains("never that the source stated it on its own"),
                "The {op} request shows a typed page label but does not explain its provenance: {instructions}"
            );
        }
    }
    assert!(checked > 0, "The test must reach the untyped product's tally question");
}

#[tokio::test]
async fn typed_page_labels_are_explained_to_count_membership() {
    check_context("count").await;
}

#[tokio::test]
async fn typed_page_labels_are_explained_to_numeric_field_selection() {
    check_context("sum").await;
}
