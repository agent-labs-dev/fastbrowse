//! A read that causes another decision must preserve the preceding pictured click in history.

use std::sync::{Arc, Mutex};

use fastbrowse_agent::agent::{Agent, RunOptions};
use fastbrowse_agent::scripted::{ScriptedJev, ScriptedLLM};
use fastbrowse_core::BoxFuture;
use fastbrowse_core::decision::{Answer, DecisionClient, DecisionError, DecisionProfile, Edges, Evaluation, Question};
use fastbrowse_core::models::Artifact;
use fastbrowse_core::page::{ActResult, Action, Capture, Observation, Page, PageResult};
use indexmap::IndexMap;
use serde_json::{Value, json};

struct PicturedPage {
    png: Vec<u8>,
    edges: Edges,
    clicked: std::sync::atomic::AtomicBool,
}

impl Page for PicturedPage {
    fn artifacts(&self) -> Vec<Artifact> {
        Vec::new()
    }

    fn observe(&self) -> BoxFuture<'_, PageResult<Observation>> {
        let text = if self.clicked.load(std::sync::atomic::Ordering::SeqCst) { "Saved popup" } else { "Editor" };
        Box::pin(async move {
            Ok(serde_json::from_value(json!({
                "url": "https://example.test/", "title": "Editor", "page_key": text, "document_key": "doc",
                "captured_at": "2026-11-18T00:00:00Z", "viewport_text": text, "tabs": [],
                "visual_content": true,
                "controls": [{
                    "id": "surface", "frame_id": null, "role": "button", "label": "button (1 of 2)",
                    "operations": ["click"], "sensitive": false, "offscreen": false, "pictured": true
                }]
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
        let text = if self.clicked.load(std::sync::atomic::Ordering::SeqCst) { "Saved popup" } else { "Editor" };
        Box::pin(async move {
            Ok(serde_json::from_value(json!({
                "url": "https://example.test/", "title": "Editor", "captured_at": "2026-11-18T00:00:00Z",
                "sha256": fastbrowse_core::py::sha256_hex(text.as_bytes()), "text": text,
                "blocks": [{"source_id": "popup", "frame_id": null, "kind": "paragraph", "start": 0, "end": text.len()}]
            }))
            .unwrap())
        })
    }

    fn act<'a>(&'a self, _: &'a Action, _: &'a Observation) -> BoxFuture<'a, PageResult<ActResult>> {
        self.clicked.store(true, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async {
            Ok(ActResult {
                outcome: fastbrowse_core::models::StepOutcome::Executed,
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
        Box::pin(async { Ok(self.png.clone()) })
    }

    fn pictures<'a>(&'a self, ids: &'a [String]) -> BoxFuture<'a, PageResult<IndexMap<String, Edges>>> {
        Box::pin(async { Ok(ids.iter().map(|id| (id.clone(), self.edges)).collect()) })
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

struct Inspect {
    inner: ScriptedJev,
    decisions: Mutex<Vec<Value>>,
}

impl DecisionClient for Inspect {
    fn profile(&self) -> DecisionProfile {
        DecisionProfile { images: true, ..DecisionProfile::jev() }
    }

    fn evaluate<'a>(
        &'a self,
        state: &'a Value,
        questions: &'a IndexMap<String, Question>,
    ) -> BoxFuture<'a, Result<Evaluation, DecisionError>> {
        if questions.contains_key("operation") {
            self.decisions.lock().unwrap().push(state.clone());
        }
        Box::pin(async move {
            let mut evaluation = self.inner.evaluate(state, questions).await?;
            if let (Some(Question::Choice(question)), Some(Answer::Choice(answer))) =
                (questions.get("step"), evaluation.answers.get_mut("step"))
                && let Some((key, _)) = question
                    .criteria
                    .iter()
                    .find(|(_, value)| value.as_str().is_some_and(|text| text.contains("triangle")))
            {
                answer.choice = key.clone();
                for (option, probability) in &mut answer.probabilities {
                    *probability = f64::from(option == key);
                }
            }
            if let Some(Answer::Noul(answer)) = evaluation.answers.get_mut("shows_answer") {
                answer.probability = 1.0;
            }
            Ok(evaluation)
        })
    }
}

#[tokio::test]
async fn test_the_decision_after_a_read_keeps_the_preceding_pictured_clicks_task_step() {
    let mut png = Vec::new();
    image::RgbaImage::new(280, 280).write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).unwrap();
    let page = Arc::new(PicturedPage {
        png,
        edges: Edges { left: 0.1, right: 0.2, top: 0.1, bottom: 0.2 },
        clicked: Default::default(),
    });
    let jev = Arc::new(Inspect {
        inner: ScriptedJev::new(
            fastbrowse_agent::scripted::steps("click button (1 of 2); click button (1 of 2)").unwrap(),
        ),
        decisions: Mutex::new(Vec::new()),
    });
    let agent = Agent::new(page, jev.clone(), Arc::new(ScriptedLLM));
    let _ = agent
        .run(
            "Click the triangle and tell me its popup.",
            RunOptions {
                start: Some("https://example.test/".into()),
                limits: Some(fastbrowse_core::models::Limits { max_steps: Some(3), ..Default::default() }),
                ..Default::default()
            },
        )
        .await;
    let decisions = jev.decisions.lock().unwrap();
    assert!(decisions.len() >= 3, "expected a decision after the read, got {decisions:?}");
    let before_read = &decisions[1]["recent_actions"][0]["target"];
    let after_read = &decisions[2]["recent_actions"][0]["target"];
    assert!(
        before_read.as_str().unwrap().contains("for the task step \"Click the triangle"),
        "pictured click has its task step: {before_read}"
    );
    assert_eq!(after_read, before_read, "reading the popup must not erase which task step opened it");
}
