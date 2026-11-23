//! A native confirm that is cancelled saves nothing, so it leaves no stale-view fact.

use fastbrowse_agent::agent::{Agent, RunOptions};
use fastbrowse_agent::scripted::{ScriptedJev, ScriptedLLM, steps};
use fastbrowse_core::BoxFuture;
use fastbrowse_core::decision::{Answer, DecisionClient, DecisionError, DecisionProfile, Evaluation, Question};
use fastbrowse_core::models::{Artifact, Authorization, Operation, StepOutcome};
use fastbrowse_core::page::{ActResult, Action, Capture, Observation, Page, PageResult};
use indexmap::IndexMap;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

struct ConfirmPage {
    stage: Mutex<usize>,
    saved_before_dialog: bool,
    kind: &'static str,
}

impl ConfirmPage {
    fn observation(&self) -> Observation {
        let stage = *self.stage.lock().unwrap();
        let dialog = stage == if self.saved_before_dialog { 3 } else { 2 };
        let text = if dialog {
            ""
        } else if stage < 2 {
            "Older replies"
        } else {
            "Your reply was saved. Older replies"
        };
        let controls = if stage < 2 {
            json!([
                {"id": "description", "frame_id": null, "role": "textbox", "label": "Description", "operations": ["fill"],
                 "value": if stage == 0 { "" } else { "Thanks" }},
                {"id": "save", "frame_id": null, "role": "button", "label": "Save", "operations": ["click"]}
            ])
        } else if self.saved_before_dialog && stage == 2 {
            json!([{"id": "save", "frame_id": null, "role": "button", "label": "Save", "operations": ["click"]}])
        } else {
            json!([])
        };
        serde_json::from_value(json!({
            "url": "https://example.test/settings", "title": "Settings",
            "document_key": if dialog { "" } else { "d1" },
            "dialog": if dialog { json!({"kind": self.kind, "message": "Save reply?", "default_prompt": null}) } else { Value::Null },
            "page_key": format!("stage-{stage}"), "captured_at": "2026-11-22T00:00:00Z",
            "viewport_text": text, "controls": controls, "tabs": []
        }))
        .unwrap()
    }
}

impl Page for ConfirmPage {
    fn artifacts(&self) -> Vec<Artifact> {
        Vec::new()
    }
    fn observe(&self) -> BoxFuture<'_, PageResult<Observation>> {
        Box::pin(async { Ok(self.observation()) })
    }
    fn document_changed<'a>(&'a self, _: &'a Observation) -> BoxFuture<'a, PageResult<bool>> {
        Box::pin(async { Ok(false) })
    }

    fn navigate<'a>(&'a self, _: &'a str, _: Option<&'a str>) -> BoxFuture<'a, PageResult<()>> {
        *self.stage.lock().unwrap() = 3;
        Box::pin(async { Ok(()) })
    }

    fn capture(&self) -> BoxFuture<'_, PageResult<Capture>> {
        Box::pin(async {
            Ok(serde_json::from_value(json!({
                "url": "https://example.test/settings", "title": "Settings",
                "captured_at": "2026-11-22T00:00:00Z", "sha256": "", "text": "Settings", "blocks": []
            }))
            .unwrap())
        })
    }

    fn act<'a>(&'a self, action: &'a Action, _: &'a Observation) -> BoxFuture<'a, PageResult<ActResult>> {
        let mut stage = self.stage.lock().unwrap();
        match action.operation {
            Operation::Fill => {
                assert_eq!(action.text.as_deref(), Some("Thanks"));
                *stage = 1;
            }
            Operation::Click => *stage += 1,
            Operation::Dialog => {
                assert_eq!(action.accept_dialog, Some(false));
                *stage = 1;
            }
            other => panic!("unexpected action: {other:?}"),
        }
        Box::pin(async {
            Ok(ActResult {
                outcome: StepOutcome::Executed,
                page_changed: true,
                failure: None,
                detail: None,
                moved: false,
                landed: None,
            })
        })
    }

    fn redrawn<'a>(&'a self, _: &'a Observation, _: f64, _: Option<&'a str>) -> BoxFuture<'a, PageResult<bool>> {
        Box::pin(async { Ok(false) })
    }

    fn withhold_frames(&self, _: bool) {}
    fn screenshot(&self) -> BoxFuture<'_, PageResult<Vec<u8>>> {
        Box::pin(async {
            let image = image::RgbaImage::from_pixel(280, 280, image::Rgba([255, 255, 255, 255]));
            let mut png = std::io::Cursor::new(Vec::new());
            image.write_to(&mut png, image::ImageFormat::Png).unwrap();
            Ok(png.into_inner())
        })
    }
    fn address(&self) -> BoxFuture<'_, PageResult<String>> {
        Box::pin(async { Ok("https://example.test/settings".into()) })
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

struct Watching {
    stop_at: usize,
    inner: ScriptedJev,
    decisions: Mutex<Vec<Value>>,
}

impl DecisionClient for Watching {
    fn profile(&self) -> DecisionProfile {
        DecisionProfile { images: false, ..DecisionProfile::jev() }
    }

    fn evaluate<'a>(
        &'a self,
        state: &'a Value,
        questions: &'a IndexMap<String, Question>,
    ) -> BoxFuture<'a, Result<Evaluation, DecisionError>> {
        Box::pin(async move {
            if questions.contains_key("operation") {
                let mut decisions = self.decisions.lock().unwrap();
                decisions.push(state.clone());
                if decisions.len() == self.stop_at {
                    return Err(DecisionError::failed("inspection finished"));
                }
            }
            let mut result = self.inner.evaluate(state, questions).await?;
            for key in ["irreversible", "lasting", "requested"] {
                if let Some(Answer::Noul(answer)) = result.answers.get_mut(key) {
                    answer.probability = 1.0;
                }
            }
            Ok(result)
        })
    }
}

#[tokio::test]
async fn cancelling_a_confirm_leaves_no_stale_page_fact() {
    let page = Arc::new(ConfirmPage { stage: Mutex::new(0), saved_before_dialog: false, kind: "confirm" });
    let jev = Arc::new(Watching {
        stop_at: 4,
        inner: ScriptedJev::new(steps("fill Description = Thanks; click Save; dialog = dismiss").unwrap()),
        decisions: Mutex::new(Vec::new()),
    });
    let agent = Agent::new(page.clone(), jev.clone(), Arc::new(ScriptedLLM));
    let _ = agent
        .run(
            "Thanks is the reply to post; confirm its save",
            RunOptions { authorization: Some(Authorization { irreversible_actions: true }), ..Default::default() },
        )
        .await;
    assert_eq!(*page.stage.lock().unwrap(), 1, "the confirm must be cancelled");
    let decisions = jev.decisions.lock().unwrap();
    assert_eq!(decisions.len(), 4);
    let facts = decisions[3]["page_facts"].to_string();
    assert!(!facts.contains("changed what the site keeps"), "a cancelled save kept nothing: {facts}");
}

#[tokio::test]
async fn dismissing_an_alert_keeps_the_stale_page_fact_of_the_save_it_reported() {
    let page = Arc::new(ConfirmPage { stage: Mutex::new(0), saved_before_dialog: false, kind: "alert" });
    let jev = Arc::new(Watching {
        stop_at: 4,
        inner: ScriptedJev::new(steps("fill Description = Thanks; click Save; dialog = dismiss").unwrap()),
        decisions: Mutex::new(Vec::new()),
    });
    let agent = Agent::new(page.clone(), jev.clone(), Arc::new(ScriptedLLM));
    let _ = agent
        .run(
            "Thanks is the reply to post; confirm its save",
            RunOptions { authorization: Some(Authorization { irreversible_actions: true }), ..Default::default() },
        )
        .await;
    let decisions = jev.decisions.lock().unwrap();
    assert_eq!(decisions.len(), 4);
    let facts = decisions[3]["page_facts"].to_string();
    assert!(facts.contains("has not been loaded since"), "the alert's save already happened: {facts}");
}
