//! Source newlines and an appended tooltip must preserve a sent reply as evidence.

use std::sync::{Arc, Mutex};

use fastbrowse_agent::agent::{Agent, RunOptions};
use fastbrowse_agent::scripted::{ScriptedDriver, ScriptedJev, steps};
use fastbrowse_core::BoxFuture;
use fastbrowse_core::config::Config;
use fastbrowse_core::decision::{Answer, DecisionClient, DecisionError, Evaluation, Question};
use fastbrowse_core::models::{Artifact, Authorization};
use fastbrowse_core::page::{ActResult, Action, Capture, Observation, Page, PageResult};
use indexmap::IndexMap;
use serde_json::{Value, json};

struct SavePage {
    saved: Mutex<bool>,
    notice_captured: Mutex<bool>,
}

impl Page for SavePage {
    fn artifacts(&self) -> Vec<Artifact> {
        Vec::new()
    }

    fn observe(&self) -> BoxFuture<'_, PageResult<Observation>> {
        let saved = *self.saved.lock().unwrap();
        // <div>Changes\nsaved\nsuccessfully <span title="Save status">now</span></div>.
        // The observation collapses source newlines; the capture appends the inline tooltip.
        let notice =
            if saved && !*self.notice_captured.lock().unwrap() { "Changes saved successfully now" } else { "Editor" };
        Box::pin(async move {
            Ok(serde_json::from_value(json!({
                "url": "https://editor.test/", "title": "Editor", "page_key": if saved {"saved"} else {"editor"},
                "document_key": "editor", "captured_at": "2026-11-22T00:00:00Z", "tabs": [],
                "viewport_text": notice,
                "controls": [{"id": "save", "frame_id": null, "role": "button", "label": "Save", "operations": ["click"]}]
            })).unwrap())
        })
    }

    fn document_changed<'a>(&'a self, _: &'a Observation) -> BoxFuture<'a, PageResult<bool>> {
        Box::pin(async { Ok(false) })
    }

    fn navigate<'a>(&'a self, _: &'a str, _: Option<&'a str>) -> BoxFuture<'a, PageResult<()>> {
        Box::pin(async { Ok(()) })
    }

    fn capture(&self) -> BoxFuture<'_, PageResult<Capture>> {
        // Like a toast, the notice is available for the capture immediately after Save, then disappears.
        let text =
            if *self.saved.lock().unwrap() && !std::mem::replace(&mut *self.notice_captured.lock().unwrap(), true) {
                "Changes\nsaved\nsuccessfully now (title: Save status)"
            } else {
                "Editor"
            };
        Box::pin(async move {
            Ok(serde_json::from_value(json!({
                "url": "https://editor.test/", "title": "Editor", "captured_at": "2026-11-22T00:00:00Z",
                "sha256": "", "text": text,
                "blocks": [{"source_id": "s0", "kind": "paragraph", "frame_id": null, "start": 0, "end": text.chars().count()}]
            })).unwrap())
        })
    }

    fn act<'a>(&'a self, _: &'a Action, _: &'a Observation) -> BoxFuture<'a, PageResult<ActResult>> {
        *self.saved.lock().unwrap() = true;
        Box::pin(async { Ok(serde_json::from_value(json!({"outcome": "executed", "page_changed": true})).unwrap()) })
    }

    fn redrawn<'a>(&'a self, _: &'a Observation, _: f64, _: Option<&'a str>) -> BoxFuture<'a, PageResult<bool>> {
        Box::pin(async { Ok(false) })
    }

    fn screenshot(&self) -> BoxFuture<'_, PageResult<Vec<u8>>> {
        Box::pin(async { Ok(Vec::new()) })
    }
    fn withhold_frames(&self, _: bool) {}
    fn address(&self) -> BoxFuture<'_, PageResult<String>> {
        Box::pin(async { Ok("https://editor.test/".into()) })
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

struct SavingJev {
    script: ScriptedJev,
}

impl DecisionClient for SavingJev {
    fn profile(&self) -> fastbrowse_core::decision::DecisionProfile {
        self.script.profile()
    }

    fn evaluate<'a>(
        &'a self,
        state: &'a Value,
        questions: &'a IndexMap<String, Question>,
    ) -> BoxFuture<'a, Result<Evaluation, DecisionError>> {
        Box::pin(async move {
            let mut evaluation = self.script.evaluate(state, questions).await?;
            for key in ["irreversible", "requested", "lasting"] {
                if let Some(Answer::Noul(answer)) = evaluation.answers.get_mut(key) {
                    answer.probability = 1.0;
                }
            }
            Ok(evaluation)
        })
    }
}

#[tokio::test(start_paused = true)]
async fn source_newlines_with_an_inline_tooltip_keep_the_reply_citable() {
    let page = Arc::new(SavePage { saved: Mutex::new(false), notice_captured: Mutex::new(false) });
    let jev = Arc::new(SavingJev { script: ScriptedJev::new(steps("click Save; done").unwrap()) });
    let config: Config = serde_json::from_value(json!({"step_frames": false})).unwrap();
    let agent =
        Agent::new(page.clone(), jev.clone(), Arc::new(ScriptedDriver::new(steps("click Save; done").unwrap())))
            .with_config(config);
    let result = agent
        .run(
            "Save the description",
            RunOptions {
                start: Some("https://editor.test/".into()),
                authorization: Some(Authorization { irreversible_actions: true }),
                ..RunOptions::default()
            },
        )
        .await
        .unwrap();
    assert!(*page.saved.lock().unwrap(), "the Save was sent");
    let cited = result.evidence.iter().map(|e| e.quote.as_str()).collect::<Vec<_>>().join("\n");
    assert!(
        cited.contains("Changes\nsaved\nsuccessfully now"),
        "the sent reply must be citable despite source newlines and the appended tooltip; notes={cited}"
    );
}
