//! Both Jev wires, their validation, splits and failover.

mod common;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use common::{Script, answering, reply, text};
use fastbrowse_clients::failover::FailoverDecisionClient;
use fastbrowse_clients::http::{Http, Request, Response, TransportError};
use fastbrowse_clients::system_one::{IMAGES_HEADER, OPENROUTER_MODEL, OPENROUTER_URL, SystemOneClient, TYPESAFE_URL};
use fastbrowse_clients::validation::{Lines, Paid, RETRY_DELAYS_SECONDS, jev_spend};
use fastbrowse_clients::vercel::{GATEWAY_URL, VercelGatewayJevClient};
use fastbrowse_core::decision::{
    Answer, BoxQuestion, ChoiceAnswer, ChoiceQuestion, DecisionClient, DecisionError, Evaluation, Images,
    JEV_DOLLARS_PER_INPUT_TOKEN, JEV_MODEL, NoulAnswer, NoulQuestion, Question, ScoreQuestion,
};
use fastbrowse_core::models::{CostBasis, CostComponent, Limits};
use fastbrowse_core::telemetry::Ledger;
use indexmap::IndexMap;
use serde_json::{Value, json};
use tokio::sync::Notify;

const ATTEMPTS: usize = RETRY_DELAYS_SECONDS.len() + 1;

fn choice() -> Question {
    ChoiceQuestion::new("Which colour?", IndexMap::from([("blue".into(), json!("Blue")), ("red".into(), json!("Red"))]))
        .into()
}

fn noul(instructions: &str) -> Question {
    NoulQuestion::new(instructions, None, None).into()
}

fn questions(pairs: impl IntoIterator<Item = (&'static str, Question)>) -> IndexMap<String, Question> {
    pairs.into_iter().map(|(key, question)| (key.to_string(), question)).collect()
}

fn choice_answer(updates: Value) -> Value {
    let mut answer =
        json!({"type": "choice", "choice": "blue", "probabilities": {"blue": 0.9, "red": 0.1}, "confidence": 0.8});
    for (key, value) in updates.as_object().unwrap() {
        answer[key] = value.clone();
    }
    answer
}

fn keys(request: &Request) -> Vec<String> {
    let body: Value = serde_json::from_slice(&request.body).unwrap();
    body["questions"].as_object().unwrap().keys().cloned().collect()
}

fn host(request: &Request) -> String {
    url::Url::parse(&request.url).unwrap().host_str().unwrap().to_string()
}

fn direct(http: Http) -> SystemOneClient {
    SystemOneClient::new("key", http, TYPESAFE_URL, JEV_MODEL).typesafe()
}

fn gateway(http: Http) -> VercelGatewayJevClient {
    VercelGatewayJevClient::new("key", http, GATEWAY_URL)
}

/// Both clients behind the trait, for a test that runs against each.
fn both(http: &Http) -> [(bool, Box<dyn DecisionClient>); 2] {
    [(false, Box::new(direct(http.clone()))), (true, Box::new(gateway(http.clone())))]
}

fn always(status: u16, body: Value) -> Http {
    answering(move |_| {
        let response = Response::json(status, &body);
        Box::pin(async move { Ok(response) })
    })
}

#[tokio::test]
async fn test_openrouter_uses_systemone_and_records_cost() {
    for (tokens, cost) in [(476, 0.00003), (0, 0.0)] {
        let http = answering(move |request| {
            assert_eq!(request.url, "https://openrouter.ai/api/v1/systemone");
            assert_eq!(request.header("authorization"), Some("Bearer openrouter-key"));
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            assert_eq!(
                body,
                json!({"model": OPENROUTER_MODEL, "state": "blue", "questions": {
                    "q": {"type": "choice", "instructions": "Which colour?", "criteria": {"blue": "Blue", "red": "Red"}}
                }})
            );
            let response = Response::json(
                200,
                &json!({"model": "typesafe/jev-1.13-20260917", "id": "gen-dec-test", "provider": "TypeSafe",
                    "answers": {"q": choice_answer(json!({}))},
                    "usage": {"input_tokens": tokens, "output_tokens": 70, "cost": cost}}),
            );
            Box::pin(async move { Ok(response) })
        });
        let client = SystemOneClient::new("openrouter-key", http, OPENROUTER_URL, OPENROUTER_MODEL);
        let result = client.evaluate(&json!("blue"), &questions([("q", choice())])).await.unwrap();
        assert_eq!(result.model, "typesafe/jev-1.13-20260917");
        assert!(matches!(result.answers["q"], Answer::Choice(_)));
        assert_eq!(result.cost.component, CostComponent::Jev);
        assert_eq!(result.cost.basis, CostBasis::Metered);
        assert!((result.cost.dollars.unwrap() - cost).abs() < 1e-12);
        assert_eq!((result.input_tokens, result.cost.input_tokens), (tokens, tokens));
        assert_eq!(result.cost.output_tokens, 70);
    }
}

/// A route that takes images is sent the image in TypeSafe's documented format with the images header, and a route
/// that does not never receives a page's pixels, nor the reference to them.
#[tokio::test]
async fn test_only_a_route_that_takes_images_is_sent_them() {
    let images = Images::from([("shot-1".to_string(), b"png bytes".to_vec())]);
    let state = json!({"page": "text", "viewport_screenshot": "shot-1"});
    for route in ["text", "openrouter", "typesafe"] {
        let sent = route != "text";
        let http = answering(move |request| {
            let flagged = request
                .headers
                .iter()
                .any(|(name, value)| name.eq_ignore_ascii_case(IMAGES_HEADER.0) && value == "true");
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            if sent {
                assert!(flagged);
                assert_eq!(
                    body["assets"],
                    json!({"shot-1": {"type": "image", "format": "png", "data_base64": "cG5nIGJ5dGVz"}})
                );
                assert_eq!(body["state"], json!({"page": "text", "viewport_screenshot": "shot-1"}));
            } else {
                assert!(!flagged);
                assert!(body.get("assets").is_none());
                assert_eq!(body["state"], json!({"page": "text"}));
            }
            let response = Response::json(
                200,
                &json!({"answers": {"q": {"type": "noul", "noul": 0.5}}, "usage": {"input_tokens": 1}}),
            );
            Box::pin(async move { Ok(response) })
        });
        let client = match route {
            "typesafe" => SystemOneClient::new("key", http, "https://direct.test", JEV_MODEL).typesafe(),
            "openrouter" => SystemOneClient::new("openrouter-key", http, OPENROUTER_URL, OPENROUTER_MODEL).images(),
            _ => SystemOneClient::new("openrouter-key", http, OPENROUTER_URL, OPENROUTER_MODEL).long_context(),
        };
        let result = client.evaluate_images(&state, &questions([("q", noul("Is it?"))]), &images).await.unwrap();
        assert!(matches!(result.answers["q"], Answer::Noul(_)));
    }
}

/// A run that fails over from TypeSafe's own API asks its backup without the image or the reference to it.
#[tokio::test]
async fn test_a_backup_route_is_asked_without_images() {
    let images = Images::from([("shot-1".to_string(), b"png bytes".to_vec())]);
    let state = json!({"page": "text", "viewport_screenshot": "shot-1"});
    let http = answering(|request| {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        let response = if request.url.starts_with("https://direct.test") {
            assert!(body.get("assets").is_some());
            Response::json(401, &json!({"error": "bad key"}))
        } else {
            assert!(!request.headers.iter().any(|(name, _)| name.eq_ignore_ascii_case(IMAGES_HEADER.0)));
            assert!(body.get("assets").is_none());
            assert_eq!(body["state"], json!({"page": "text"}));
            Response::json(200, &json!({"answers": {"q": {"type": "noul", "noul": 0.5}}, "usage": {"input_tokens": 1}}))
        };
        Box::pin(async move { Ok(response) })
    });
    let client = FailoverDecisionClient::new(
        Box::new(SystemOneClient::new("key", http.clone(), "https://direct.test", JEV_MODEL).typesafe()),
        Box::new(SystemOneClient::new("openrouter-key", http.clone(), OPENROUTER_URL, OPENROUTER_MODEL)),
        http,
    );
    let result = client.evaluate_images(&state, &questions([("q", noul("Is it?"))]), &images).await.unwrap();
    assert!(matches!(result.answers["q"], Answer::Noul(_)));
}

/// A request with no image never asks for image input, even on TypeSafe's own API.
#[tokio::test]
async fn test_a_request_without_images_carries_no_image_input() {
    let http = answering(|request| {
        assert!(!request.headers.iter().any(|(name, _)| name.eq_ignore_ascii_case(IMAGES_HEADER.0)));
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        assert!(body.get("assets").is_none());
        let response = Response::json(
            200,
            &json!({"answers": {"q": {"type": "noul", "noul": 0.5}}, "usage": {"input_tokens": 1}}),
        );
        Box::pin(async move { Ok(response) })
    });
    let client = SystemOneClient::new("key", http, "https://direct.test", JEV_MODEL).typesafe();
    client.evaluate(&json!("page"), &questions([("q", noul("Is it?"))])).await.unwrap();
}

/// A box question goes out with its instructions and no criteria, and its answer comes back as four edges and a
/// confidence; edges that cross are not a box.
#[tokio::test]
async fn test_a_box_is_asked_without_criteria_and_read_as_its_edges() {
    let asked = questions([(
        "where",
        BoxQuestion { kind: Default::default(), instructions: "The mug in `photo`".into() }.into(),
    )]);
    let edges = json!({"top": 0.1, "bottom": 0.3, "left": 0.2, "right": 0.6});
    for (edges, confidence, valid) in [
        (edges.clone(), json!(0.8), Some(Some(0.8))),
        // TypeSafe's own example answer carries a null confidence.
        (edges, Value::Null, Some(None)),
        (json!({"top": 0.5, "bottom": 0.3, "left": 0.2, "right": 0.6}), json!(0.8), None),
    ] {
        let http = answering(move |request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            assert_eq!(body["questions"], json!({"where": {"type": "box", "instructions": "The mug in `photo`"}}));
            let response = Response::json(
                200,
                &json!({"answers": {"where": {"type": "box", "box": edges, "confidence": confidence}}, "usage": {"input_tokens": 1}}),
            );
            Box::pin(async move { Ok(response) })
        });
        let client = SystemOneClient::new("key", http, "https://direct.test", JEV_MODEL).typesafe();
        let result = client.evaluate(&json!({"photo": "p"}), &asked).await;
        if let Some(expected) = valid {
            let Answer::Box(found) = &result.unwrap().answers["where"] else { panic!("a box") };
            assert_eq!((found.edges.centre(), found.confidence), ((0.4, 0.2), expected));
        } else {
            assert!(result.unwrap_err().to_string().contains("box edges cross"));
        }
    }
}

#[tokio::test]
async fn test_systemone_rejects_invalid_cost() {
    for cost in [json!(-1), json!(true), json!("NaN"), json!("inf"), json!("not-a-cost")] {
        let http = always(
            200,
            json!({"answers": {"q": choice_answer(json!({}))}, "usage": {"input_tokens": 100, "cost": cost}}),
        );
        let client = SystemOneClient::new("key", http, OPENROUTER_URL, OPENROUTER_MODEL);
        let error = client.evaluate(&json!("blue"), &questions([("q", choice())])).await.unwrap_err();
        assert!(error.to_string().contains("Invalid Jev response"), "{cost}: {error}");
    }
}

#[tokio::test]
async fn test_direct_encodes_all_question_types_and_estimates_usage() {
    let asked = questions([
        ("col", choice()),
        ("bare", noul("Wrong?")),
        ("green", NoulQuestion::new("Green?", Some("green"), Some("not green")).into()),
        ("partial", NoulQuestion::new("Present?", Some("present"), None).into()),
        (
            "score",
            ScoreQuestion {
                kind: Default::default(),
                instructions: "How much?".into(),
                criteria: vec!["none".into(), "some".into(), "all".into()],
            }
            .into(),
        ),
    ]);
    let http = answering(|request| {
        assert_eq!(request.url, "https://direct.test/v1/systemone");
        assert_eq!(request.header("authorization"), Some("Bearer key"));
        let payload: Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(payload["state"], json!({"text": "hello"}));
        assert_eq!(payload["model"], "jev-1.13.1");
        assert_eq!(
            payload["questions"],
            json!({
                "col": {"type": "choice", "instructions": "Which colour?", "criteria": {"blue": "Blue", "red": "Red"}},
                "bare": {"type": "noul", "instructions": "Wrong?"},
                "green": {"type": "noul", "instructions": "Green?",
                    "criteria": {"true": "green", "false": "not green"}},
                "partial": {"type": "noul", "instructions": "Present?", "criteria": {"true": "present"}},
                "score": {"type": "score", "instructions": "How much?", "criteria": ["none", "some", "all"]},
            })
        );
        let response = Response::json(
            200,
            &json!({"answers": {
                "col": choice_answer(json!({})),
                "bare": {"type": "noul", "noul": 0},
                "green": {"type": "noul", "noul": 0.02},
                "partial": {"type": "noul", "noul": 1},
                "score": {"type": "score", "score": 1.99, "probabilities": {"0": 0, "1": 0, "2": 1},
                    "confidence": 0.99},
            }, "usage": {"input_tokens": 389}}),
        );
        Box::pin(async move { Ok(response) })
    });
    let client = SystemOneClient::new("key", http, "https://direct.test/", "jev-1.13.1");
    let result = client.evaluate(&json!({"text": "hello"}), &asked).await.unwrap();
    assert_eq!(result.cost.basis, CostBasis::Estimated);
    assert!((result.cost.dollars.unwrap() - 389.0 * 0.042 / 1_000_000.0).abs() < 1e-15);
    assert_eq!((result.cost.input_tokens, result.input_tokens), (389, 389));
    assert!(matches!(result.answers["col"], Answer::Choice(_)));
    assert_eq!(result.answers["green"], Answer::Noul(NoulAnswer { kind: Default::default(), probability: 0.02 }));
    let Answer::Score(score) = &result.answers["score"] else { panic!("a score") };
    assert_eq!(score.score, 1.99);
}

#[tokio::test]
async fn test_an_unmetered_answer_from_a_model_with_no_price_is_an_unknown_cost() {
    let http = answering(|_| {
        let response = Response::json(
            200,
            &json!({"answers": {"q": {"type": "noul", "noul": 0.5}}, "usage": {"input_tokens": 389}}),
        );
        Box::pin(async move { Ok(response) })
    });
    let client = SystemOneClient::new("", http, "http://localhost:11434", "nimble");
    let asked = IndexMap::from([("q".to_string(), NoulQuestion::new("Green?", None, None).into())]);
    let result = client.evaluate(&json!("hello"), &asked).await.unwrap();
    assert_eq!((result.cost.basis, result.cost.dollars), (CostBasis::Unknown, None));
    assert_eq!(client.profile().max_choice_options, 26);
}

#[tokio::test]
async fn test_direct_rejects_invalid_answers() {
    for answer in [
        choice_answer(json!({"choice": "absent"})),
        choice_answer(json!({"probabilities": {"blue": 1}})),
        choice_answer(json!({"probabilities": {"blue": 1, "red": 0, "extra": 0}})),
        choice_answer(json!({"probabilities": {"blue": 0.3, "red": 0.3}})),
        choice_answer(json!({"probabilities": {"blue": 1.1, "red": -0.1}})),
        choice_answer(json!({"probabilities": {"blue": "0.9", "red": 0.1}})),
        choice_answer(json!({"probabilities": {"blue": true, "red": 0}})),
        choice_answer(json!({"confidence": null})),
        choice_answer(json!({"confidence": 1.01})),
        choice_answer(json!({"type": "noul"})),
    ] {
        let http = always(200, json!({"answers": {"q": answer}, "usage": {"input_tokens": 10}}));
        let error = direct(http).evaluate(&json!("state"), &questions([("q", choice())])).await.unwrap_err();
        assert!(error.to_string().contains("HTTP 200"), "{answer}: {error}");
    }
}

#[tokio::test]
async fn test_nonfinite_probabilities_are_rejected() {
    for invalid in ["NaN", "Infinity", "-Infinity"] {
        let body = [
            r#"{"answers":{"q":{"type":"choice","choice":"blue","probabilities":{"blue":"#,
            invalid,
            r#","red":0},"confidence":1}},"usage":{"input_tokens":1}}"#,
        ]
        .concat();
        let script = Script::new([text(200, &body)]);
        assert!(direct(script.http()).evaluate(&json!("state"), &questions([("q", choice())])).await.is_err());
    }
}

#[tokio::test]
async fn test_unused_speculative_head_is_validated_and_missing_head_rejected() {
    let http = always(200, json!({"answers": {"q": choice_answer(json!({}))}, "usage": {"input_tokens": 10}}));
    let error =
        direct(http).evaluate(&json!("state"), &questions([("q", choice()), ("unused", choice())])).await.unwrap_err();
    assert!(error.to_string().contains("answer ids"), "{error}");
}

#[tokio::test(start_paused = true)]
async fn test_http_failures_and_token_limits() {
    for (status, body, too_large) in [
        (400, "max_tokens_exceeded", true),
        (413, "", true),
        (500, "max_tokens_exceeded", false),
        (401, "bad key", false),
    ] {
        let body = format!("{body}{}", "x".repeat(2000));
        let http = answering(move |_| {
            let response = Response::new(status, body.clone());
            Box::pin(async move { Ok(response) })
        });
        for (_, client) in both(&http) {
            let error = client.evaluate(&json!("state"), &questions([("q", choice())])).await.unwrap_err();
            assert_eq!(matches!(error, DecisionError::InputTooLarge(_)), too_large, "{status}");
            assert!(error.to_string().contains(&status.to_string()));
            assert!(error.to_string().len() < 600);
        }
    }
}

/// A gateway failure as one arrives: its own wrapper, the provider's reason nested inside, and its routing.
fn gateway_error(status: u16, reason: &str, retried: bool) -> Value {
    json!({
        "error": {"message": format!("typesafe returned status {status}"), "type": "AI_APICallError",
            "param": {"message": reason, "statusCode": status, "isRetryable": retried}},
        "providerMetadata": {"gateway": {"routing": {"resolvedProvider": "typesafe-ai", "fallbacksAvailable": []}}},
    })
}

/// A gateway answer, whose confidence rides in provider metadata rather than on the answer itself.
fn gateway_answer() -> Value {
    json!({
        "answers": {"q": {"type": "choice", "choice": "blue", "probabilities": {"blue": 1, "red": 0}}},
        "rounding": {"probabilityDecimals": 2, "scoreDecimals": 2},
        "usage": {"inputTokens": 389, "outputTokens": 61},
        "warnings": [],
        "providerMetadata": {"typesafe": {"confidence": {"q": 1}}, "gateway": {"cost": "0.000016338"}},
    })
}

#[tokio::test]
async fn test_a_refusal_over_an_input_limit_is_input_too_large() {
    // Either wording a gateway relays for a request past a limit has to reach the shed path, not a failed run.
    for reason in ["max_tokens_exceeded", "request body too large"] {
        let http = always(400, gateway_error(400, reason, false));
        let error = gateway(http).evaluate(&json!("state"), &questions([("q", choice())])).await.unwrap_err();
        assert!(matches!(error, DecisionError::InputTooLarge(_)) && error.to_string().contains("input too large"));
    }
}

#[tokio::test(start_paused = true)]
async fn test_a_gateway_failure_names_what_the_provider_said() {
    // The wrapper repeats the gateway's own status, so the reason worth reading is the one nested inside it.
    let http = always(503, gateway_error(503, "the model is unavailable", true));
    let error = gateway(http).evaluate(&json!("state"), &questions([("q", choice())])).await.unwrap_err();
    assert!(matches!(error, DecisionError::RetriesExhausted(_)));
    assert!(error.to_string().contains("the model is unavailable"));
    assert!(error.to_string().contains("via typesafe-ai"));
}

#[tokio::test(start_paused = true)]
async fn test_a_status_the_gateway_marks_retryable_is_repeated() {
    // 409 is retryable to the AI SDK and is not a status the set names, so the flag alone has to carry it.
    let script =
        Script::new([reply(409, gateway_error(409, "the request conflicts", true)), reply(200, gateway_answer())]);
    let result = gateway(script.http()).evaluate(&json!("state"), &questions([("q", choice())])).await.unwrap();
    assert_eq!(script.requests().len(), 2);
    assert!(matches!(result.answers["q"], Answer::Choice(_)));
}

#[tokio::test]
async fn test_a_status_the_gateway_marks_not_retryable_is_not_repeated() {
    // A refused request is not repeated on the chance it clears: the gateway says the request is at fault.
    let script = Script::new([reply(409, gateway_error(409, "the request conflicts", false))]);
    assert!(gateway(script.http()).evaluate(&json!("state"), &questions([("q", choice())])).await.is_err());
    assert_eq!(script.requests().len(), 1);
}

#[tokio::test]
async fn test_gateway_remaps_wire_types_confidence_and_cost() {
    for cost in [json!("0.000016338"), json!(0.000016338), json!(0), Value::Null] {
        let wire = cost.clone();
        let http = answering(move |request| {
            assert_eq!(request.url, "https://ai-gateway.vercel.sh/v4/ai/evaluation-model");
            for (key, value) in [
                ("authorization", "Bearer key"),
                ("ai-gateway-auth-method", "api-key"),
                ("ai-gateway-protocol-version", "0.0.1"),
                ("ai-model-id", "typesafe-ai/jev-1.14.0"),
                ("ai-evaluation-model-specification-version", "4"),
            ] {
                assert_eq!(request.header(key), Some(value));
            }
            let payload: Value = serde_json::from_slice(&request.body).unwrap();
            assert_eq!(payload.as_object().unwrap().keys().collect::<Vec<_>>(), ["state", "questions"]);
            assert_eq!(payload["questions"]["green"], json!({"instructions": "Green?", "type": "boolean"}));
            let response = Response::json(
                200,
                &json!({
                    "answers": {
                        "green": {"type": "boolean", "probability": 0.02},
                        "col": {"type": "choice", "choice": "blue", "probabilities": {"blue": 1, "red": 0}},
                        "sc": {"type": "score", "score": 1.99, "probabilities": {"0": 0, "1": 0, "2": 1}},
                    },
                    "rounding": {"probabilityDecimals": 2, "scoreDecimals": 2},
                    "usage": {"inputTokens": 389, "outputTokens": 61},
                    "warnings": [],
                    "providerMetadata": {"typesafe": {"confidence": {"col": 1, "sc": 0.99}}, "gateway": {"cost": wire}},
                }),
            );
            Box::pin(async move { Ok(response) })
        });
        let score = ScoreQuestion {
            kind: Default::default(),
            instructions: "Score".into(),
            criteria: vec!["low".into(), "medium".into(), "high".into()],
        };
        let result = gateway(http)
            .evaluate(&json!("state"), &questions([("green", noul("Green?")), ("col", choice()), ("sc", score.into())]))
            .await
            .unwrap();
        assert_eq!(result.answers["green"], Answer::Noul(NoulAnswer { kind: Default::default(), probability: 0.02 }));
        let Answer::Choice(selected) = &result.answers["col"] else { panic!("a choice") };
        assert_eq!(selected.confidence, 1.0);
        assert_eq!(result.input_tokens, 389);
        assert_eq!(result.cost.output_tokens, 61);
        // The gateway metering $0 while 389 tokens flowed is not a free request: it is priced at list, as unmetered.
        let metered = match &cost {
            Value::String(text) => text.parse::<f64>().unwrap(),
            Value::Number(number) => number.as_f64().unwrap(),
            _ => 0.0,
        };
        let listed = metered == 0.0;
        assert_eq!(result.cost.basis, if listed { CostBasis::Estimated } else { CostBasis::Metered });
        let expected = if listed { 389.0 * 0.042 / 1_000_000.0 } else { metered };
        assert!((result.cost.dollars.unwrap() - expected).abs() < 1e-15);
    }
}

#[tokio::test]
async fn test_score_distributions_are_validated() {
    for probabilities in
        [json!({"0": 0.2, "1": 0.8}), json!({"0": 0, "1": 0, "2": 0.8}), json!({"0": -1, "1": 1, "2": 1})]
    {
        let http = always(
            200,
            json!({"answers": {"q": {"type": "score", "score": 1.3, "probabilities": probabilities, "confidence": 0.8}},
                "usage": {"input_tokens": 1}}),
        );
        let score = ScoreQuestion {
            kind: Default::default(),
            instructions: "Score".into(),
            criteria: vec!["a".into(), "b".into(), "c".into()],
        };
        assert!(direct(http).evaluate(&json!("state"), &questions([("q", score.into())])).await.is_err());
    }
}

#[tokio::test]
async fn test_probability_rounding_boundary_and_near_ties_are_accepted() {
    let http = always(
        200,
        json!({"answers": {"q": choice_answer(json!({"probabilities": {"blue": 0.49, "red": 0.49}}))},
            "usage": {"input_tokens": 1}}),
    );
    let result = direct(http).evaluate(&json!("state"), &questions([("q", choice())])).await.unwrap();
    assert!(matches!(result.answers["q"], Answer::Choice(_)));
}

#[tokio::test(start_paused = true)]
async fn test_transport_error_is_wrapped_without_exposing_key() {
    // A transport error is named by its type alone, so nothing it carried can reach the message.
    let http = answering(|_| Box::pin(async { Err(TransportError::Transient("ConnectError")) }));
    let error = direct(http).evaluate(&json!("state"), &questions([("q", choice())])).await.unwrap_err();
    assert!(matches!(error, DecisionError::TransportFailed(_)));
    assert!(error.to_string().contains("transport failed"), "{error}");
}

#[tokio::test]
async fn test_invalid_speculative_answer_is_not_ignored() {
    let http = always(
        200,
        json!({"answers": {"chosen_head": choice_answer(json!({})),
            "unused_head": choice_answer(json!({"choice": "absent"}))},
            "usage": {"input_tokens": 10}}),
    );
    let error = direct(http)
        .evaluate(&json!("state"), &questions([("chosen_head", choice()), ("unused_head", choice())]))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("unused_head"), "{error}");
}

#[tokio::test]
async fn test_gateway_requires_metadata_confidence() {
    let http = always(200, json!({"answers": {"q": choice_answer(json!({}))}, "usage": {"inputTokens": 10}}));
    assert!(gateway(http).evaluate(&json!("state"), &questions([("q", choice())])).await.is_err());
}

#[tokio::test]
async fn test_noul_probability_is_validated() {
    for probability in [json!(-0.1), json!(1.1), json!("0.2"), json!(true)] {
        let http =
            always(200, json!({"answers": {"q": {"type": "noul", "noul": probability}}, "usage": {"input_tokens": 1}}));
        assert!(direct(http).evaluate(&json!("state"), &questions([("q", noul("Wrong?"))])).await.is_err());
    }
}

#[tokio::test]
async fn test_a_choice_one_rounding_unit_below_the_top_is_accepted() {
    // Sum-to-one rounding put escalate 0.40 as the choice over read 0.41 on a live run.
    let answer = choice_answer(json!({"choice": "red", "probabilities": {"blue": 0.5, "red": 0.49}}));
    let http = always(200, json!({"answers": {"q": answer}, "usage": {"input_tokens": 10}}));
    let evaluation = direct(http).evaluate(&json!("state"), &questions([("q", choice())])).await.unwrap();
    let Answer::Choice(result) = &evaluation.answers["q"] else { panic!("a choice") };
    assert_eq!(result.choice, "red");
}

#[tokio::test]
async fn test_an_echoed_key_never_reaches_the_error() {
    let key = format!("sk-{}", "a1b2c3d4".repeat(6));
    // An echoed key straddling where the message is cut, and a body that fails validation.
    let bodies = [
        (401, json!({"error": {"type": "auth", "message": format!("{} bad key {key}", "x".repeat(270))}})),
        (200, json!(key)),
    ];
    for (status, payload) in bodies {
        let http = always(status, payload);
        let clients: [Box<dyn DecisionClient>; 2] = [
            Box::new(SystemOneClient::new(key.clone(), http.clone(), TYPESAFE_URL, JEV_MODEL).typesafe()),
            Box::new(VercelGatewayJevClient::new(key.clone(), http.clone(), GATEWAY_URL)),
        ];
        for client in clients {
            let error = client.evaluate(&json!("state"), &questions([("q", choice())])).await.unwrap_err();
            assert!(!error.to_string().contains("a1b2c3d4a1b2"), "{error}");
        }
    }
}

#[tokio::test]
async fn test_a_choice_of_one_option_is_answered_without_asking() {
    // Jev refuses a choice of one option, and wiki-godel retried that refusal as an outage for hours.
    let only: Question = ChoiceQuestion::new("Which field?", IndexMap::from([("e7".into(), json!("Search"))])).into();
    for gateway_route in [false, true] {
        let sent: Arc<Mutex<Vec<Vec<String>>>> = Arc::default();
        let seen = sent.clone();
        let http = answering(move |request| {
            seen.lock().unwrap().push(keys(request));
            let response = Response::json(
                200,
                &json!({"answers": {"col": choice_answer(json!({}))}, "usage": {"input_tokens": 9, "inputTokens": 9},
                    "providerMetadata": {"typesafe": {"confidence": {"col": 0.8}}}}),
            );
            Box::pin(async move { Ok(response) })
        });
        let client: Box<dyn DecisionClient> = if gateway_route {
            Box::new(VercelGatewayJevClient::new("key", http, "https://gateway.test"))
        } else {
            Box::new(SystemOneClient::new("key", http, "https://direct.test", "jev-1.13.0"))
        };
        let mixed =
            client.evaluate(&json!({}), &questions([("col", choice()), ("fill_target", only.clone())])).await.unwrap();
        let alone = client.evaluate(&json!({}), &questions([("fill_target", only.clone())])).await.unwrap();
        assert_eq!(*sent.lock().unwrap(), vec![vec!["col".to_string()]]);
        for result in [&mixed, &alone] {
            assert_eq!(
                result.answers["fill_target"],
                Answer::Choice(ChoiceAnswer {
                    kind: Default::default(),
                    choice: "e7".into(),
                    probabilities: IndexMap::from([("e7".into(), 1.0)]),
                    confidence: 1.0
                })
            );
        }
        assert_eq!(mixed.answers.keys().collect::<Vec<_>>(), ["col", "fill_target"]);
        assert_eq!(alone.input_tokens, 0);
    }
}

fn noul_answers(keys: &[String], gateway_route: bool) -> Value {
    let answers: serde_json::Map<String, Value> = keys
        .iter()
        .map(|key| {
            let answer = if gateway_route {
                json!({"type": "boolean", "probability": 1})
            } else {
                json!({"type": "noul", "noul": 1})
            };
            (key.clone(), answer)
        })
        .collect();
    json!({"answers": answers, "usage": {if gateway_route { "inputTokens" } else { "input_tokens" }: 100}})
}

#[tokio::test(start_paused = true)]
async fn test_failed_batch_splits_with_only_remaining_attempts() {
    for gateway_route in [false, true] {
        for exhaust in [false, true] {
            for retried_questions in [0, 3, 10] {
                let seen: Arc<Mutex<HashMap<Vec<String>, usize>>> = Arc::default();
                let counts = seen.clone();
                let http = answering(move |request| {
                    let keys = keys(request);
                    let count = {
                        let mut counts = counts.lock().unwrap();
                        let count = counts.entry(keys.clone()).or_default();
                        *count += 1;
                        *count
                    };
                    let response = if keys.len() > 1 {
                        Response::new(503, "")
                    } else {
                        let index: usize = keys[0][1..].parse().unwrap();
                        let first = count == 1 && index < retried_questions;
                        if first || (exhaust && keys[0] == "q9") {
                            Response::new(503, "")
                        } else {
                            Response::json(200, &noul_answers(&keys, gateway_route))
                        }
                    };
                    Box::pin(async move { Ok(response) })
                });
                let client: Box<dyn DecisionClient> =
                    if gateway_route { Box::new(gateway(http)) } else { Box::new(direct(http)) };
                let asked: IndexMap<String, Question> = (0..10).map(|i| (format!("q{i}"), noul("x"))).collect();
                let total = || seen.lock().unwrap().values().sum::<usize>();
                match client.evaluate(&json!("page"), &asked).await {
                    Err(DecisionError::RetriesExhausted(error)) => {
                        assert!(exhaust || retried_questions > ATTEMPTS - 3);
                        assert_eq!(error.requests as usize, total());
                    }
                    Err(other) => panic!("{other}"),
                    Ok(result) => {
                        assert!(!exhaust);
                        for key in asked.keys() {
                            assert_eq!(
                                result.answers[key],
                                Answer::Noul(NoulAnswer { kind: Default::default(), probability: 1.0 })
                            );
                        }
                        assert_eq!(result.input_tokens, 1000);
                        assert_eq!(result.cost.input_tokens, 1000);
                        assert_eq!(result.requests as usize, total());
                        let expected = result.cost.input_tokens as f64 * 0.042 / 1_000_000.0;
                        assert!((result.cost.dollars.unwrap() - expected).abs() < 1e-12);
                    }
                }
                let batch: Vec<String> = asked.keys().cloned().collect();
                assert_eq!(seen.lock().unwrap()[&batch], 2);
                assert!(total() - 2 <= ATTEMPTS - 2 + asked.len());
            }
        }
    }
}

#[tokio::test(start_paused = true)]
async fn test_failover_exhaustion_preserves_answered_singles_and_request_counts() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let http = answering(move |request| {
        counter.fetch_add(1, Ordering::SeqCst);
        let answered = if host(request) == "primary.test" { "a" } else { "b" };
        let response = if keys(request) != [answered] {
            Response::new(503, "")
        } else {
            Response::json(
                200,
                &json!({"answers": {answered: {"type": "noul", "noul": 1}}, "usage": {"input_tokens": 100}}),
            )
        };
        Box::pin(async move { Ok(response) })
    });
    let client = FailoverDecisionClient::new(
        Box::new(SystemOneClient::new("key", http.clone(), "https://primary.test", "jev-1.13.0")),
        Box::new(SystemOneClient::new("key", http.clone(), "https://backup.test", "jev-1.13.0")),
        http,
    );
    let paid = Paid::default();
    let asked = questions([("a", noul("a")), ("b", noul("b")), ("c", noul("c"))]);
    let error = jev_spend(Lines::Paid(paid.clone()), None, client.evaluate(&json!("page"), &asked)).await.unwrap_err();
    let DecisionError::RetriesExhausted(error) = error else { panic!("exhausted") };
    assert_eq!(error.requests as usize, calls.load(Ordering::SeqCst));
    let answered: Vec<&String> = error.answered.iter().flat_map(|result| result.answers.keys()).collect();
    assert_eq!(answered, ["a", "b"]);
    assert_eq!(paid.lines().iter().map(|line| line.input_tokens).sum::<i64>(), 200);
}

#[tokio::test(start_paused = true)]
async fn test_batch_split_does_not_change_other_failure_contracts() {
    for status in [400, 429, 200] {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let http = answering(move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
            let body = if status == 400 { json!({"error": "max_tokens_exceeded"}) } else { json!({}) };
            let response = Response::json(status, &body);
            Box::pin(async move { Ok(response) })
        });
        let asked = questions([("a", noul("a")), ("b", noul("b"))]);
        for (_, client) in both(&http) {
            calls.store(0, Ordering::SeqCst);
            let error = client.evaluate(&json!("page"), &asked).await.unwrap_err();
            match status {
                400 => assert!(matches!(error, DecisionError::InputTooLarge(_))),
                429 => assert!(matches!(error, DecisionError::RetriesExhausted(_))),
                _ => assert!(matches!(error, DecisionError::Failed(_))),
            }
            assert_eq!(calls.load(Ordering::SeqCst), if status == 429 { ATTEMPTS } else { 1 });
        }
    }
}

#[tokio::test(start_paused = true)]
async fn test_a_split_single_that_would_cross_the_dollar_cap_is_not_sent() {
    let cap = 0.001;
    let script_sent: Arc<Mutex<Vec<Vec<String>>>> = Arc::default();
    let sent = script_sent.clone();
    let http = answering(move |request| {
        let keys = keys(request);
        sent.lock().unwrap().push(keys.clone());
        // The first single's bill leaves less room under the cap than the next single's own input.
        let billed = (cap / JEV_DOLLARS_PER_INPUT_TOKEN) as i64 - 100;
        let response = if keys.len() > 1 {
            Response::new(503, "")
        } else {
            Response::json(
                200,
                &json!({"answers": {&keys[0]: {"type": "noul", "noul": 1}}, "usage": {"input_tokens": billed}}),
            )
        };
        Box::pin(async move { Ok(response) })
    });
    let ledger = Arc::new(Ledger::new(Limits { max_dollars: Some(cap), ..Limits::default() }));
    let asked = questions([("a", noul(&"a".repeat(3000))), ("b", noul(&"b".repeat(3000)))]);
    let client = direct(http);
    let result =
        jev_spend(Lines::Ledger(ledger.clone()), Some(ledger.clone()), client.evaluate(&json!("page"), &asked)).await;
    assert!(matches!(result, Err(DecisionError::Budget(_))), "{result:?}");
    let a = vec!["a".to_string()];
    let both_keys = vec!["a".to_string(), "b".to_string()];
    assert_eq!(*script_sent.lock().unwrap(), vec![both_keys.clone(), both_keys, a]);
}

#[tokio::test(start_paused = true)]
async fn test_split_batch_keeps_paid_singles_on_cancellation_and_failover() {
    for cancel in [true, false] {
        let seen: Counts = Arc::default();
        let waiting = Arc::new(Notify::new());
        let stopped = Arc::new(AtomicBool::new(false));
        let (counts, wake, stop) = (seen.clone(), waiting.clone(), stopped.clone());
        let http = answering(move |request| {
            let keys = keys(request);
            let host = host(request);
            *counts.lock().unwrap().entry((host.clone(), keys.clone())).or_default() += 1;
            let (wake, stop) = (wake.clone(), stop.clone());
            Box::pin(async move {
                if host == "primary.test" && (keys.len() > 1 || keys == ["b"]) {
                    if cancel && keys == ["b"] {
                        let _flag = SetOnDrop(stop);
                        wake.notify_one();
                        std::future::pending::<()>().await;
                    }
                    return Ok(Response::new(503, ""));
                }
                let answers: serde_json::Map<String, Value> =
                    keys.iter().map(|key| (key.clone(), json!({"type": "noul", "noul": 1}))).collect();
                Ok(Response::json(200, &json!({"answers": answers, "usage": {"input_tokens": 100}})))
            })
        });
        let primary = SystemOneClient::new("key", http.clone(), "https://primary.test", "jev-1.13.0");
        let client: Box<dyn DecisionClient> = if cancel {
            Box::new(primary)
        } else {
            let backup = SystemOneClient::new("key", http.clone(), "https://backup.test", "jev-1.13.0");
            Box::new(FailoverDecisionClient::new(Box::new(primary), Box::new(backup), http.clone()))
        };
        let paid = Paid::default();
        let asked = questions([("a", noul("a")), ("b", noul("b"))]);
        let page = json!("page");
        let evaluating = jev_spend(Lines::Paid(paid.clone()), None, client.evaluate(&page, &asked));
        let count = |host: &str, key: &str| {
            seen.lock().unwrap().get(&(host.to_string(), vec![key.to_string()])).copied().unwrap_or(0)
        };
        if cancel {
            tokio::select! {
                _ = evaluating => panic!("the call should have been cancelled"),
                _ = waiting.notified() => {}
            }
            assert!(stopped.load(Ordering::SeqCst));
            let lines = paid.lines();
            assert_eq!(lines.len(), 2);
            assert_eq!(lines[0].input_tokens, 100);
            assert_eq!(lines[1].basis, CostBasis::Estimated);
            assert!(lines[1].input_tokens > 0);
            assert_eq!(lines[1].dollars, Some(lines[1].input_tokens as f64 * JEV_DOLLARS_PER_INPUT_TOKEN));
        } else {
            let result: Evaluation = evaluating.await.unwrap();
            for key in ["a", "b"] {
                assert_eq!(
                    result.answers[key],
                    Answer::Noul(NoulAnswer { kind: Default::default(), probability: 1.0 })
                );
            }
            assert_eq!((result.input_tokens, result.cost.input_tokens), (200, 200));
            assert!(paid.is_empty());
            assert_eq!(count("backup.test", "b"), 1);
        }
        assert_eq!(count("primary.test", "a"), 1);
        assert_eq!(count("backup.test", "a"), 0);
    }
}

/// Requests seen per host and question keys.
type Counts = Arc<Mutex<HashMap<(String, Vec<String>), usize>>>;

struct SetOnDrop(Arc<AtomicBool>);

impl Drop for SetOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[tokio::test(start_paused = true)]
async fn test_split_singletons_share_the_remaining_request_budget() {
    for gateway_route in [false, true] {
        let seen: Arc<Mutex<HashMap<Vec<String>, usize>>> = Arc::default();
        let counts = seen.clone();
        let http = answering(move |request| {
            *counts.lock().unwrap().entry(keys(request)).or_default() += 1;
            Box::pin(async move { Ok(Response::new(503, "")) })
        });
        let client: Box<dyn DecisionClient> =
            if gateway_route { Box::new(gateway(http)) } else { Box::new(direct(http)) };
        let error =
            client.evaluate(&json!("page"), &questions([("a", noul("a")), ("b", noul("b"))])).await.unwrap_err();
        assert!(matches!(error, DecisionError::RetriesExhausted(_)));
        let seen = seen.lock().unwrap();
        assert_eq!(seen[&vec!["a".to_string(), "b".to_string()]], 2);
        assert_eq!(seen[&vec!["a".to_string()]], RETRY_DELAYS_SECONDS.len() - 1);
        assert!(!seen.contains_key(&vec!["b".to_string()]));
    }
}

#[tokio::test(start_paused = true)]
async fn test_a_refused_key_moves_jev_to_the_backup_at_once() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let http = answering(move |request| {
        counter.fetch_add(1, Ordering::SeqCst);
        let response = if host(request) == "primary.test" {
            Response::json(401, &json!({"error": {"message": "User not found."}}))
        } else {
            Response::json(200, &json!({"answers": {"a": {"type": "noul", "noul": 1}}, "usage": {"input_tokens": 100}}))
        };
        Box::pin(async move { Ok(response) })
    });
    let client = FailoverDecisionClient::new(
        Box::new(SystemOneClient::new("key", http.clone(), "https://primary.test", "jev-1.13.0")),
        Box::new(SystemOneClient::new("key", http.clone(), "https://backup.test", "jev-1.13.0")),
        http.clone(),
    );
    let asked = questions([("a", noul("a"))]);
    let result = client.evaluate(&json!("page"), &asked).await.unwrap();
    assert_eq!(result.answers.keys().collect::<Vec<_>>(), ["a"]);
    // A refusal is not retried: one request to the primary, one to the backup.
    assert_eq!(calls.load(Ordering::SeqCst), 2);

    let alone = SystemOneClient::new("key", http, "https://primary.test", "jev-1.13.0");
    let error = alone.evaluate(&json!("page"), &asked).await.unwrap_err();
    assert!(matches!(error, DecisionError::Rejected(_)), "{error}");
    assert!(error.to_string().ends_with("(primary.test rejected the API key)"), "{error}");
}
