//! A card's outline can identify a control even when its crop fills the viewport.

use std::sync::{Arc, Mutex};

use fastbrowse_agent::agent::{Agent, RunOptions};
use fastbrowse_agent::scripted::{ScriptedJev, ScriptedLLM};
use fastbrowse_core::BoxFuture;
use fastbrowse_core::decision::{DecisionClient, DecisionError, DecisionProfile, Edges, Evaluation, Images, Question};
use fastbrowse_core::models::Artifact;
use fastbrowse_core::page::{ActResult, Action, Capture, Observation, Page, PageResult};
use indexmap::IndexMap;
use serde_json::{Value, json};

struct PicturedPage {
    png: Vec<u8>,
    edges: Edges,
}

impl Page for PicturedPage {
    fn artifacts(&self) -> Vec<Artifact> {
        Vec::new()
    }

    fn observe(&self) -> BoxFuture<'_, PageResult<Observation>> {
        Box::pin(async {
            Ok(serde_json::from_value(json!({
                "url": "https://example.test/", "title": "Editor", "page_key": "page",
                "captured_at": "2026-11-18T00:00:00Z", "viewport_text": "Editor", "tabs": [],
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
        Box::pin(async {
            Ok(serde_json::from_value(json!({
                "url": "https://example.test/", "title": "Editor", "captured_at": "2026-11-18T00:00:00Z",
                "sha256": "", "text": "Editor", "blocks": []
            }))
            .unwrap())
        })
    }

    fn act<'a>(&'a self, _: &'a Action, _: &'a Observation) -> BoxFuture<'a, PageResult<ActResult>> {
        unreachable!("the test stops at the action question")
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
    action: Mutex<Option<(IndexMap<String, Question>, Images)>>,
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
        self.inner.evaluate(state, questions)
    }

    fn evaluate_images<'a>(
        &'a self,
        state: &'a Value,
        questions: &'a IndexMap<String, Question>,
        images: &'a Images,
    ) -> BoxFuture<'a, Result<Evaluation, DecisionError>> {
        if questions.contains_key("operation") {
            assert!(state.get("viewport_screenshot").is_some());
            *self.action.lock().unwrap() = Some((questions.clone(), images.clone()));
            return Box::pin(async { Err(DecisionError::Failed("inspection finished".into())) });
        }
        self.inner.evaluate(state, questions)
    }
}

#[tokio::test]
async fn test_a_viewport_sized_crop_keeps_the_outline_that_identifies_its_control() {
    // Jev accepts a 280-pixel viewport. The minimum card size expands a small icon to that whole viewport, while its
    // outline still identifies the icon.
    let mut png = Vec::new();
    image::RgbaImage::new(280, 280).write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).unwrap();
    let page = Arc::new(PicturedPage { png, edges: Edges { left: 0.1, right: 0.2, top: 0.1, bottom: 0.2 } });
    let jev = Arc::new(Inspect { inner: ScriptedJev::new(Vec::new()), action: Mutex::new(None) });
    let agent = Agent::new(page, jev.clone(), Arc::new(ScriptedLLM));
    let _ = agent.run("click the pictured control", RunOptions::default()).await;
    let action = jev.action.lock().unwrap();
    let (questions, images) = action.as_ref().expect("the action question was inspected");
    let Question::Choice(target) = &questions["click_target"] else { panic!("a target choice") };
    let reference = target.criteria["surface"]["looks_like"]
        .as_str()
        .expect("a card with a visible outline must remain beside the screenshot");
    let card = image::load_from_memory(&images[reference]).unwrap().to_rgba8();
    assert_eq!((card.width(), card.height()), (280, 280));
    assert_eq!(*card.get_pixel(35, 27), image::Rgba([255, 0, 255, 255]));
}
