//! tests/clients/test_call_spans.py: every dispatched model call leaves its span in the trace, however it ended. A
//! failed or cancelled call is billed too, and missing from the trace it made that time look free.

mod common;

use std::collections::HashMap;

use common::{Script, Step, answer_model, reply, text};
use fastbrowse_clients::openai_compatible::{LLMOptions, OpenAICompatibleLLM};
use fastbrowse_clients::system_one::SystemOneClient;
use fastbrowse_clients::vercel::VercelGatewayJevClient;
use fastbrowse_core::decision::{DecisionClient, NoulQuestion, Question};
use fastbrowse_core::llm::{LLMClient, Message};
use fastbrowse_core::models::LLMPurpose;
use fastbrowse_core::telemetry::Trace;
use indexmap::IndexMap;
use serde_json::{Value, json};

fn spans(trace: &Trace) -> Vec<(String, String)> {
    let events = trace.events();
    let spans: Vec<&Value> = events.iter().filter(|e| e["event"] == "call").collect();
    assert!(spans.iter().all(|s| s["ended"].as_f64() >= s["began"].as_f64()));
    spans.iter().map(|s| (s["call"].as_str().unwrap().into(), s["outcome"].as_str().unwrap().into())).collect()
}

fn llm(script: &Script, trace: &Trace) -> OpenAICompatibleLLM {
    let models = HashMap::from([(LLMPurpose::Read, "m".to_string())]);
    OpenAICompatibleLLM::new("key", script.traced(trace), "https://llm.test", models, LLMOptions::default()).unwrap()
}

#[tokio::test(start_paused = true)]
async fn test_llm_calls_are_traced_when_they_fail_or_are_cancelled() {
    let trace = Trace::collecting();
    let messages = [Message::user("Answer.")];
    let schema = answer_model();
    let broken = || reply(200, json!({"choices": [{"message": {"content": "not json"}}], "usage": {"cost": 0.0}}));
    let failing = Script::new([broken(), broken()]);
    assert!(llm(&failing, &trace).generate_value(LLMPurpose::Read, &messages, &schema, 2000, None).await.is_err());
    // Python cancels the task; here the future is dropped once its request is in flight.
    let hanging = Script::new([Step::Cancel]);
    let client = llm(&hanging, &trace);
    tokio::select! {
        _ = client.generate_value(LLMPurpose::Read, &messages, &schema, 2000, None) => panic!("not cancelled"),
        _ = hanging.cancel.notified() => {}
    }
    assert_eq!(spans(&trace), [("llm:read".into(), "error".into()), ("llm:read".into(), "cancelled".into())]);
}

#[tokio::test(start_paused = true)]
async fn test_jev_calls_are_traced_when_they_fail_are_cancelled_or_answer_invalidly() {
    // A 200 whose body is not a valid answer is a failed call, not a successful one.
    let trace = Trace::collecting();
    let question: IndexMap<String, Question> =
        IndexMap::from([("q".to_string(), NoulQuestion::new("Is it?", Some("Yes."), Some("No.")).into())]);
    let state = json!({});
    let direct = |script: &Script| SystemOneClient::new("key", script.traced(&trace), "https://jev.test", "jev");
    let unauthorized = Script::new([text(401, "")]);
    assert!(direct(&unauthorized).evaluate(&state, &question).await.is_err());
    let invalid = Script::new([reply(200, json!({"answers": "x"}))]);
    assert!(direct(&invalid).evaluate(&state, &question).await.is_err());
    let not_json = Script::new([text(200, "not json")]);
    let gateway = VercelGatewayJevClient::new("key", not_json.traced(&trace), "https://gateway.test");
    assert!(gateway.evaluate(&state, &question).await.is_err());
    let hanging = Script::new([Step::Cancel]);
    let client = direct(&hanging);
    tokio::select! {
        _ = client.evaluate(&state, &question) => panic!("not cancelled"),
        _ = hanging.cancel.notified() => {}
    }
    let valid =
        json!({"answers": {"q": {"type": "noul", "noul": 1}}, "usage": {"input_tokens": 10, "output_tokens": 1}});
    let answered = Script::new([reply(200, valid)]);
    direct(&answered).evaluate(&state, &question).await.unwrap();
    let expected =
        ["error", "error", "error", "cancelled", "ok"].map(|outcome| ("jev".to_string(), outcome.to_string()));
    assert_eq!(spans(&trace), expected);
}

#[tokio::test(start_paused = true)]
async fn test_a_model_call_leaves_its_request_and_reply_in_the_trace_without_its_key() {
    let trace = Trace::collecting();
    let content = json!({"answer": "yes"}).to_string();
    let script = Script::new([reply(200, json!({"choices": [{"message": {"content": content}}], "note": "key"}))]);
    let (client, schema) = (llm(&script, &trace), answer_model());
    assert!(client.generate_value(LLMPurpose::Read, &[Message::user("Answer.")], &schema, 2000, None).await.is_ok());
    let events = trace.events();
    let http: Vec<&Value> = events.iter().filter(|e| e["event"] == "http").collect();
    assert_eq!(http.len(), 1);
    assert_eq!(http[0]["status"], 200);
    assert_eq!(http[0]["request"]["messages"][0]["content"], "Answer.");
    assert_eq!(http[0]["response"]["note"], "[api key]");
}
