//! A switch saved by its click, and a control judged by its pixels, record their effect before the next observation
//! diffs the page, and left the next decision without the fact that the page may not show what they changed.

use std::sync::{Arc, Mutex};

use fastbrowse_agent::agent::{Agent, RunOptions};
use fastbrowse_agent::scripted::{ScriptedJev, ScriptedLLM, steps};
use fastbrowse_core::BoxFuture;
use fastbrowse_core::decision::{Answer, DecisionClient, DecisionError, DecisionProfile, Evaluation, Question};
use fastbrowse_core::models::{Artifact, Authorization, Operation, StepOutcome};
use fastbrowse_core::page::{ActResult, Action, Capture, Observation, Page, PageResult};
use indexmap::IndexMap;
use serde_json::{Value, json};

struct SettingsPage {
    saved: Mutex<bool>,
    drawn: bool,
}

impl SettingsPage {
    fn observation(&self) -> Observation {
        let saved = *self.saved.lock().unwrap();
        serde_json::from_value(json!({
            "url": "https://example.test/settings", "title": "Settings", "document_key": "d1",
            "page_key": if saved && !self.drawn { "saved" } else { "initial" },
            "captured_at": "2026-11-22T00:00:00Z", "viewport_text": "Settings", "tabs": [],
            "controls": [{
                "id": "notifications", "frame_id": null,
                "role": if self.drawn { "button" } else { "switch" }, "label": "Notifications",
                "operations": ["click"], "checked": (!self.drawn).then_some(saved),
                "sensitive": false, "offscreen": false,
                "drawn": self.drawn
            }]
        }))
        .unwrap()
    }
}

impl Page for SettingsPage {
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
        assert_eq!(action.operation, Operation::Click);
        *self.saved.lock().unwrap() = true;
        Box::pin(async {
            Ok(ActResult {
                outcome: StepOutcome::Executed,
                page_changed: !self.drawn,
                failure: None,
                detail: None,
                moved: false,
            })
        })
    }

    fn redrawn<'a>(&'a self, _: &'a Observation, _: f64, _: Option<&'a str>) -> BoxFuture<'a, PageResult<bool>> {
        Box::pin(async { Ok(false) })
    }

    fn screenshot(&self) -> BoxFuture<'_, PageResult<Vec<u8>>> {
        Box::pin(async {
            let colour = if *self.saved.lock().unwrap() { 255 } else { 0 };
            let image = image::RgbaImage::from_pixel(280, 280, image::Rgba([colour, 0, 0, 255]));
            let mut png = std::io::Cursor::new(Vec::new());
            image.write_to(&mut png, image::ImageFormat::Png).unwrap();
            Ok(png.into_inner())
        })
    }
    fn withhold_frames(&self, _: bool) {}
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
    inner: ScriptedJev,
    decisions: Mutex<Vec<Value>>,
    drawn: bool,
}

impl DecisionClient for Watching {
    fn profile(&self) -> DecisionProfile {
        DecisionProfile { images: self.drawn, ..DecisionProfile::jev() }
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
                if decisions.len() == 2 {
                    return Err(DecisionError::failed("inspection finished"));
                }
            }
            let mut result = self.inner.evaluate(state, questions).await?;
            for key in ["lasting", "requested", "changed"] {
                if let Some(Answer::Noul(answer)) = result.answers.get_mut(key) {
                    answer.probability = 1.0;
                }
            }
            Ok(result)
        })
    }
}

async fn check_saved_fact(drawn: bool) {
    let page = Arc::new(SettingsPage { saved: Mutex::new(false), drawn });
    let jev = Arc::new(Watching {
        inner: ScriptedJev::new(steps("click Notifications").unwrap()),
        decisions: Mutex::new(Vec::new()),
        drawn,
    });
    let agent = Agent::new(page.clone(), jev.clone(), Arc::new(ScriptedLLM));
    let _ = agent
        .run(
            "Turn on notifications",
            RunOptions { authorization: Some(Authorization { irreversible_actions: true }), ..Default::default() },
        )
        .await;
    assert!(*page.saved.lock().unwrap(), "the lasting setting change must execute");
    let decisions = jev.decisions.lock().unwrap();
    assert_eq!(decisions.len(), 2, "inspect the next decision");
    assert!(
        decisions[1]["page_facts"].as_array().unwrap().iter().any(|fact| {
            fact.as_str().is_some_and(|fact| {
                fact.contains("click 'Notifications' changed what the site keeps")
                    && fact.contains("has not been loaded since")
            })
        }),
        "the setting save needs the stale-page fact: {}",
        decisions[1]["page_facts"]
    );
}

#[tokio::test]
async fn a_lasting_setting_change_is_stated_before_the_next_decision() {
    check_saved_fact(false).await;
}

#[tokio::test]
async fn a_lasting_change_judged_by_its_pixels_is_stated_before_the_next_decision() {
    check_saved_fact(true).await;
}
