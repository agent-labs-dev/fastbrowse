//! A fact kept for the steps must remain typeable when its page exceeds one request.

use std::sync::{Arc, Mutex};

use fastbrowse_agent::agent::{Agent, RunOptions};
use fastbrowse_agent::scripted::{ScriptedJev, ScriptedLLM, Step};
use fastbrowse_core::BoxFuture;
use fastbrowse_core::decision::{Answer, DecisionClient, DecisionError, DecisionProfile, Evaluation, Question};
use fastbrowse_core::models::{Artifact, Operation, StepOutcome};
use fastbrowse_core::page::{ActResult, Action, Capture, Observation, Page, PageResult};
use indexmap::IndexMap;
use serde_json::{Value, json};

struct CodePage {
    capture: Capture,
    typed: Mutex<Option<String>>,
}

impl Page for CodePage {
    fn artifacts(&self) -> Vec<Artifact> {
        Vec::new()
    }
    fn observe(&self) -> BoxFuture<'_, PageResult<Observation>> {
        Box::pin(async {
            Ok(serde_json::from_value(json!({
                "url": "https://example.test/", "title": "Code", "page_key": "code",
                "document_key": "document", "captured_at": "2026-11-18T00:00:00Z",
                "viewport_text": "Your code is 4821", "tabs": [],
                "controls": [{"id": "code", "frame_id": null, "role": "textbox", "label": "Code",
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
                landed: None,
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

struct ReadCode {
    inner: ScriptedJev,
    read_for_steps: Mutex<bool>,
    selected_read_source: Mutex<bool>,
    comparisons: Mutex<usize>,
}

impl DecisionClient for ReadCode {
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
                        let chosen = if question.criteria.contains_key("steps") && key == "task" {
                            *self.read_for_steps.lock().unwrap() = true;
                            Some("steps")
                        } else if question.criteria.contains_key("read") {
                            *self.selected_read_source.lock().unwrap() = true;
                            Some("read")
                        } else if key == "begins" || key == "ends" {
                            candidate_key(question, |value| value.contains("4821") || value.contains("7315"))
                                .or(Some("none"))
                        } else if (key == "first" || key == "last") && question.criteria.contains_key("none") {
                            candidate_key(question, |value| value.starts_with("'4821'") || value.starts_with("'7315'"))
                                .or(Some("none"))
                        } else if key == "read_candidate" {
                            *self.comparisons.lock().unwrap() += 1;
                            assert!(candidate_key(question, |value| value == "4821").is_some());
                            Some(candidate_key(question, |value| value == "7315").expect("second exact code"))
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

fn candidate_key(question: &fastbrowse_core::decision::ChoiceQuestion, matches: impl Fn(&str) -> bool) -> Option<&str> {
    question
        .criteria
        .iter()
        .find_map(|(key, value)| value.as_str().filter(|value| matches(value)).map(|_| key.as_str()))
}

async fn type_code_from_read(block_count: usize, compare: bool) {
    let mut text = String::new();
    let mut blocks = Vec::new();
    for n in 0..block_count {
        let start = text.len();
        text.push_str(if n == 0 {
            "4821 is a previous code. "
        } else if compare && n == block_count - 1 {
            "7315 is your code. "
        } else {
            "Reference information. "
        });
        text.push_str(&"word ".repeat(30));
        let end = text.len();
        blocks.push(
            json!({"source_id": format!("s{n}"), "kind": "paragraph", "frame_id": null, "start": start, "end": end}),
        );
        text.push_str("\n\n");
    }
    let capture: Capture = serde_json::from_value(json!({
        "url": "https://example.test/", "title": "Code", "captured_at": "2026-11-18T00:00:00Z",
        "sha256": "", "text": text, "blocks": blocks
    }))
    .unwrap();
    let page = Arc::new(CodePage { capture, typed: Mutex::new(None) });
    let jev = Arc::new(ReadCode {
        inner: ScriptedJev::new(vec![
            Step { operation: Operation::Read, label: String::new(), pick: None },
            Step { operation: Operation::Fill, label: "Code".into(), pick: None },
        ]),
        read_for_steps: Mutex::new(false),
        selected_read_source: Mutex::new(false),
        comparisons: Mutex::new(0),
    });
    let agent = Agent::new(page.clone(), jev.clone(), Arc::new(ScriptedLLM));
    let result = agent.run("Read the code and type it in Code", RunOptions::default()).await;
    assert!(*jev.read_for_steps.lock().unwrap(), "the page was kept for the steps");
    assert!(*jev.selected_read_source.lock().unwrap(), "Jev selected the read as the fill's source");
    let result = result.unwrap();
    assert_eq!(
        page.typed.lock().unwrap().as_deref(),
        Some(if compare { "7315" } else { "4821" }),
        "the read's code never reached the fill: status={:?}, error={:?}",
        result.status,
        result.error
    );
    assert_eq!(*jev.comparisons.lock().unwrap(), usize::from(compare));
}

#[tokio::test]
async fn a_fill_can_locate_a_code_in_a_short_page_read_for_the_steps() {
    type_code_from_read(2, false).await;
}

#[tokio::test]
async fn a_fill_can_locate_a_code_in_a_long_page_read_for_the_steps() {
    type_code_from_read(400, true).await;
}
