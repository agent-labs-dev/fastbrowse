//! A page's stated extreme must survive numeric candidate narrowing.

use fastbrowse_agent::memory::{Fact, Notes};
use fastbrowse_agent::retrieval::{compose, page_quotes};
use fastbrowse_core::BoxFuture;
use fastbrowse_core::decision::{
    Answer, ChoiceAnswer, ChoiceTag, DecisionClient, DecisionError, DecisionProfile, Evaluation, NoulAnswer, NoulTag,
    Question,
};
use fastbrowse_core::llm::{Generation, LLMClient, LLMError, Message, ResponseSchema};
use fastbrowse_core::models::{CostBasis, CostComponent, CostLine, FactReader, LLMPurpose};
use fastbrowse_core::page::Capture;
use fastbrowse_core::telemetry::Ledger;
use indexmap::IndexMap;
use serde_json::{Value, json};

struct LiteralJudge;

impl DecisionClient for LiteralJudge {
    fn profile(&self) -> DecisionProfile {
        DecisionProfile::jev()
    }

    fn evaluate<'a>(
        &'a self,
        _: &'a Value,
        questions: &'a IndexMap<String, Question>,
    ) -> BoxFuture<'a, Result<Evaluation, DecisionError>> {
        let answers = questions
            .iter()
            .map(|(key, question)| {
                let answer = match question {
                    Question::Noul(_) => Answer::Noul(NoulAnswer { kind: NoulTag, probability: 1.0 }),
                    Question::Choice(choice) => {
                        // The summary is not one flight and cannot truthfully select a record's own price.
                        // Select its stated starting price if offered, including an offer of a group containing it.
                        let selected = if choice.instructions.contains("Cheapest from") {
                            choice
                                .criteria
                                .iter()
                                .find(|(_, criterion)| {
                                    let words = criterion.as_str().unwrap_or_default();
                                    words.contains("$864") && words.contains("across several")
                                })
                                .map(|(key, _)| key.as_str())
                                .unwrap_or("no_record")
                        } else if choice.criteria.contains_key("n0") {
                            "n0"
                        } else {
                            choice.criteria.keys().next().unwrap().as_str()
                        };
                        assert!(choice.criteria.contains_key(selected), "missing option {selected}: {choice:?}");
                        Answer::Choice(ChoiceAnswer {
                            kind: ChoiceTag,
                            choice: selected.into(),
                            probabilities: IndexMap::from([(selected.into(), 1.0)]),
                            confidence: 1.0,
                        })
                    }
                    _ => panic!("unexpected question"),
                };
                (key.clone(), answer)
            })
            .collect();
        Box::pin(async move {
            Ok(Evaluation {
                model: "literal".into(),
                answers,
                input_tokens: 1,
                cost: CostLine::new(CostComponent::Jev, CostBasis::Estimated, Some(0.0)),
                requests: 1,
            })
        })
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
            let draft = json!({"claims": [{"text": [{"computed": true}], "evidence_ids": ["e1"], "tally": {"op": "min", "of": "flight", "field": "price", "written": "$1,087"}}], "columns": null, "numbered": false});
            Ok(Generation {
                data: schema.validate_json(&draft.to_string()).map_err(LLMError::schema)?,
                cost: CostLine::new(CostComponent::Llm, CostBasis::Estimated, Some(0.0)),
            })
        })
    }
}

async fn answer(extra_numbers: usize) -> fastbrowse_agent::retrieval::ComposedAnswer {
    let summary = format!(
        "Cheapest from $864. Available departure slots: {}",
        (10000..10000 + extra_numbers).map(|n| n.to_string()).collect::<Vec<_>>().join("; ")
    );
    let record = "Flight Alpha costs $1,087";
    let text = format!("{record}\n\n{summary}");
    let capture: Capture = serde_json::from_value(json!({"url": "https://flights.test/", "title": "Flights", "captured_at": "2026-11-22T00:00:00Z", "sha256": "capture", "text": text, "blocks": [
        {"source_id": "record", "kind": "record", "frame_id": null, "start": 0, "end": record.len(), "whole_record": true},
        {"source_id": "summary", "kind": "paragraph", "frame_id": null, "start": record.len() + 2, "end": text.len()}
    ]})).unwrap();
    let mut notes =
        Notes::new(page_quotes(&capture).into_iter().map(|e| Fact::read(&e.quote.clone(), e, FactReader::JevChoice)));
    notes.remember_capture(&capture, 0);
    compose(&LiteralJudge, &Minimum, "Find the minimum flight price", &notes, None, &[]).await.unwrap()
}

#[tokio::test]
async fn a_stated_lower_price_still_refuses_the_minimum_when_the_summary_has_many_numbers() {
    let short = answer(0).await;
    assert!(short.problem.is_some(), "the short summary must contradict the listed minimum: {short:?}");
    let long = answer(130).await;
    assert!(
        long.problem.is_some(),
        "the same stated $864 lower price must refuse $1,087 after narrowing too: {long:?}"
    );
}
