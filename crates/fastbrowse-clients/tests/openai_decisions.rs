//! OpenAI's Decisions API: the questions as it takes them, and its answers as every other client returns them.

mod common;

use common::{Script, reply};
use fastbrowse_clients::openai_decisions::{OPENAI_URL, OpenAIDecisionsClient};
use fastbrowse_core::decision::{
    Answer, ChoiceQuestion, DecisionClient, DecisionError, NoulQuestion, Question, ScoreQuestion, ScoreTag,
};
use fastbrowse_core::models::CostBasis;
use indexmap::IndexMap;
use serde_json::{Value, json};

fn asked() -> IndexMap<String, Question> {
    IndexMap::from([
        (
            "col".to_string(),
            ChoiceQuestion::new(
                "Which colour?",
                IndexMap::from([("blue".to_string(), json!("Blue")), ("b3".to_string(), json!({"label": "Red"}))]),
            )
            .into(),
        ),
        ("green".to_string(), NoulQuestion::new("Green?", Some("green"), Some("not green")).into()),
        (
            "much".to_string(),
            ScoreQuestion {
                kind: ScoreTag,
                instructions: "How much?".into(),
                criteria: vec!["none".into(), "all".into()],
            }
            .into(),
        ),
    ])
}

fn answered() -> Value {
    json!({
        "model": "gpt-6-luna-2026-10-01",
        "answers": [
            {"type": "choice", "name": "col", "choice": "blue", "confidence": 0.7,
                "probabilities": [{"value": "blue", "probability": 0.9}, {"value": "b3", "probability": 0.1}]},
            {"type": "predicate", "name": "green", "probability": 0.02},
            {"type": "score", "name": "much", "score": 0.8, "confidence": 0.55,
                "probabilities": [{"value": 0, "label": "0", "probability": 0.2},
                    {"value": 1, "label": "1", "probability": 0.8}]},
        ],
        "usage": {"input_tokens": 1000, "input_tokens_details": {"cached_tokens": 0}},
    })
}

#[tokio::test]
async fn the_questions_go_as_the_decisions_api_takes_them_and_the_answers_come_back_as_system_ones() {
    let script = Script::new([reply(200, answered())]);
    let client = OpenAIDecisionsClient::new("sk", script.http(), OPENAI_URL, "gpt-6-luna");
    let result = client.evaluate(&json!({"text": "hello"}), &asked()).await.unwrap();

    let request = &script.requests()[0];
    assert_eq!(request.url, "https://api.openai.com/v1/decisions");
    assert_eq!(request.header("authorization"), Some("Bearer sk"));
    assert_eq!(
        script.bodies()[0],
        json!({
            "model": "gpt-6-luna",
            "input": [{"role": "user", "content": [{"type": "input_json", "data": {"text": "hello"}}]}],
            "questions": [
                {"type": "choice", "name": "col", "instructions": "Which colour?",
                    "choices": [{"value": "blue", "description": "Blue"},
                        {"value": "b3", "description": "{\"label\": \"Red\"}"}]},
                {"type": "predicate", "name": "green",
                    "instructions": "Green?\nYes means: green\nNo means: not green"},
                {"type": "score", "name": "much", "instructions": "How much?",
                    "levels": [{"label": "0", "description": "none"}, {"label": "1", "description": "all"}]},
            ],
        })
    );

    assert_eq!(result.model, "gpt-6-luna-2026-10-01");
    let Answer::Choice(colour) = &result.answers["col"] else { panic!("a choice") };
    assert_eq!((colour.choice.as_str(), colour.probabilities["b3"]), ("blue", 0.1));
    assert_eq!(result.answers["green"].probability(), Some(0.02));
    let Answer::Score(much) = &result.answers["much"] else { panic!("a score") };
    assert_eq!((much.score, much.probabilities["1"]), (0.8, 0.8));
    // No cost on the wire: list price, $0.10 a million input tokens.
    assert_eq!(result.cost.basis, CostBasis::Estimated);
    assert!((result.cost.dollars.unwrap() - 1000.0 * 0.10 / 1_000_000.0).abs() < 1e-15);
}

#[tokio::test]
async fn text_state_is_sent_as_text() {
    let mut answer = answered();
    answer["answers"] = json!([{"type": "predicate", "name": "green", "probability": 1.0}]);
    let script = Script::new([reply(200, answer)]);
    let client = OpenAIDecisionsClient::new("sk", script.http(), OPENAI_URL, "gpt-6-luna");
    let questions = IndexMap::from([("green".to_string(), NoulQuestion::new("Green?", None, None).into())]);
    client.evaluate(&json!("a green page"), &questions).await.unwrap();
    assert_eq!(script.bodies()[0]["input"], "a green page");
    assert_eq!(script.bodies()[0]["questions"][0]["instructions"], "Green?");
}

#[tokio::test]
async fn a_refusal_or_a_missing_answer_is_an_invalid_response() {
    let mut refused = answered();
    refused["answers"][1] = json!({"type": "refusal", "name": "green"});
    let mut missing = answered();
    missing["answers"].as_array_mut().unwrap().pop();
    for (body, expected) in [(refused, "refused green"), (missing, "answer ids differ")] {
        let script = Script::new([reply(200, body)]);
        let client = OpenAIDecisionsClient::new("sk", script.http(), OPENAI_URL, "gpt-6-luna");
        let error = client.evaluate(&json!("page"), &asked()).await.unwrap_err();
        assert!(matches!(error, DecisionError::Failed(_)) && error.to_string().contains(expected), "{error}");
    }
}

#[test]
fn luna_takes_four_hundred_questions_and_images_and_has_no_tuned_thresholds() {
    let profile = OpenAIDecisionsClient::new("sk", Script::new([]).http(), OPENAI_URL, "gpt-6-luna").profile();
    assert_eq!((profile.max_questions, profile.images, profile.validated()), (Some(400), true, false));
}
