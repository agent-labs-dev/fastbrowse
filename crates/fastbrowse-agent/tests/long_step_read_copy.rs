//! A value crossing read windows must remain copyable as a whole.

use std::sync::{Arc, Mutex};

use fastbrowse_agent::agent::{Agent, RunOptions};
use fastbrowse_agent::scripted::{ScriptedJev, ScriptedLLM, Step};
use fastbrowse_core::BoxFuture;
use fastbrowse_core::decision::{Answer, DecisionClient, DecisionError, DecisionProfile, Evaluation, Question};
use fastbrowse_core::models::{Artifact, Operation, StepOutcome};
use fastbrowse_core::page::{ActResult, Action, Capture, Observation, Page, PageResult};
use indexmap::IndexMap;
use serde_json::{Value, json};

struct MessagePage {
    capture: Capture,
    typed: Mutex<Option<String>>,
}

impl Page for MessagePage {
    fn artifacts(&self) -> Vec<Artifact> {
        Vec::new()
    }
    fn observe(&self) -> BoxFuture<'_, PageResult<Observation>> {
        Box::pin(async {
            Ok(serde_json::from_value(json!({
                "url": "https://example.test/", "title": "Message", "page_key": "code",
                "document_key": "document", "captured_at": "2026-11-18T00:00:00Z",
                "viewport_text": "Message to copy", "tabs": [],
                "controls": [{"id": "code", "frame_id": null, "role": "textbox", "label": "Message",
                    "operations": ["fill"], "sensitive": false, "offscreen": false}]
            }))
            .unwrap())
        })
    }
    fn document_changed<'a>(&'a self, _: &'a Observation) -> BoxFuture<'a, PageResult<bool>> {
        Box::pin(async { Ok(false) })
    }
    fn navigate<'a>(&'a self, _: &'a str, _: Option<&'a str>) -> BoxFuture<'a, PageResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn capture(&self) -> BoxFuture<'_, PageResult<Capture>> {
        Box::pin(async { Ok(self.capture.clone()) })
    }
    fn act<'a>(&'a self, action: &'a Action, _: &'a Observation) -> BoxFuture<'a, PageResult<ActResult>> {
        if action.operation == Operation::Fill {
            *self.typed.lock().unwrap() = action.text.clone();
        }
        Box::pin(async {
            Ok(ActResult {
                outcome: StepOutcome::Executed,
                failure: None,
                page_changed: true,
                detail: None,
                moved: false,
            })
        })
    }
    fn redrawn<'a>(&'a self, _: &'a Observation, _: f64, _: Option<&'a str>) -> BoxFuture<'a, PageResult<bool>> {
        Box::pin(async { Ok(false) })
    }
    fn screenshot(&self) -> BoxFuture<'_, PageResult<Vec<u8>>> {
        Box::pin(async { Ok(Vec::new()) })
    }
    fn withhold_frames(&self, _: bool) {}
    fn address(&self) -> BoxFuture<'_, PageResult<String>> {
        Box::pin(async { Ok("https://example.test/".into()) })
    }
    fn response_status(&self) -> BoxFuture<'_, PageResult<Option<i64>>> {
        Box::pin(async { Ok(Some(200)) })
    }
    fn lifecycle_mark(&self) -> u64 {
        0
    }
    fn wait_for_lifecycle(&self, _: u64, _: f64) -> BoxFuture<'_, bool> {
        Box::pin(async { false })
    }
}

struct CopyMessage {
    inner: ScriptedJev,
    read_for_steps: Mutex<bool>,
    selected_read_source: Mutex<bool>,
}

impl DecisionClient for CopyMessage {
    fn profile(&self) -> DecisionProfile {
        DecisionProfile::jev()
    }
    fn evaluate<'a>(
        &'a self,
        state: &'a Value,
        questions: &'a IndexMap<String, Question>,
    ) -> BoxFuture<'a, Result<Evaluation, DecisionError>> {
        Box::pin(async move {
            let mut evaluation = self.inner.evaluate(state, questions).await?;
            for (key, question) in questions {
                match (&mut evaluation.answers[key], question) {
                    (Answer::Choice(answer), Question::Choice(question)) => {
                        let chosen = if key == "task" && question.criteria.contains_key("steps") {
                            *self.read_for_steps.lock().unwrap() = true;
                            Some("steps")
                        } else if question.criteria.contains_key("read") {
                            *self.selected_read_source.lock().unwrap() = true;
                            Some("read")
                        } else if key == "begins" || key == "ends" {
                            let boundary = if key == "begins" { "BEGIN" } else { "END" };
                            question
                                .criteria
                                .iter()
                                .find_map(|(key, value)| {
                                    value.as_str().filter(|value| value.contains(boundary)).map(|_| key.as_str())
                                })
                                .or(Some("none"))
                        } else if key == "first" || key == "last" {
                            let boundary = if key == "first" { "'BEGIN'" } else { "'END'" };
                            question
                                .criteria
                                .iter()
                                .find_map(|(key, value)| {
                                    value.as_str().filter(|value| value.starts_with(boundary)).map(|_| key.as_str())
                                })
                                .or(Some("none"))
                        } else if question.instructions.contains("Which exact text") || key == "read_candidate" {
                            question.criteria.iter().find_map(|(key, value)| {
                                value
                                    .as_str()
                                    .filter(|value| value.starts_with("BEGIN") && value.ends_with("END"))
                                    .map(|_| key.as_str())
                            })
                        } else {
                            None
                        };
                        if let Some(chosen) = chosen {
                            answer.choice = chosen.into();
                            answer.probabilities =
                                question.criteria.keys().map(|key| (key.clone(), f64::from(key == chosen))).collect();
                        }
                    }
                    (Answer::Noul(answer), _) if key == "shows_answer" || key.starts_with('w') => {
                        answer.probability = 1.0;
                    }
                    _ => {}
                }
            }
            Ok(evaluation)
        })
    }
}

async fn copy_message(block_count: usize) {
    let mut text = "BEGIN ".to_string();
    let mut blocks = Vec::new();
    for n in 0..block_count {
        let start = if n == 0 { 0 } else { text.len() };
        text.push_str("message ".repeat(30).trim_end());
        if n == block_count - 1 {
            text.push_str(" END");
        }
        blocks.push(json!({"source_id": format!("s{n}"), "kind": "paragraph",
            "frame_id": null, "start": start, "end": text.len()}));
        if n != block_count - 1 {
            text.push('\n');
        }
    }
    let capture: Capture = serde_json::from_value(json!({
        "url": "https://example.test/", "title": "Message", "captured_at": "2026-11-18T00:00:00Z",
        "sha256": "", "text": text, "blocks": blocks
    }))
    .unwrap();
    let page = Arc::new(MessagePage { capture, typed: Mutex::new(None) });
    let jev = Arc::new(CopyMessage {
        inner: ScriptedJev::new(vec![
            Step { operation: Operation::Read, label: String::new(), pick: None },
            Step { operation: Operation::Fill, label: "Message".into(), pick: None },
        ]),
        read_for_steps: Mutex::new(false),
        selected_read_source: Mutex::new(false),
    });
    let agent = Agent::new(page.clone(), jev.clone(), Arc::new(ScriptedLLM));
    let result = agent
        .run("Copy the complete message from BEGIN through END into Message", RunOptions::default())
        .await
        .unwrap();
    assert!(*jev.read_for_steps.lock().unwrap(), "the read kept the message for the steps");
    assert!(*jev.selected_read_source.lock().unwrap(), "the fill selected what was read");
    let typed = page.typed.lock().unwrap();
    assert!(
        typed.as_deref() == Some(text.as_str()),
        "the complete message was not typed: expected {} chars, got {:?}; status={:?}, error={:?}",
        text.len(),
        typed.as_ref().map(String::len),
        result.status,
        result.error
    );
}

#[tokio::test]
async fn a_short_message_read_for_steps_can_be_copied_whole() {
    copy_message(2).await;
}

#[tokio::test]
async fn a_message_spanning_read_windows_can_be_copied_whole() {
    copy_message(130).await;
}
