//! Saving on a long page must not stall the chat loop while aligning its notice.

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
}

impl Page for SavePage {
    fn artifacts(&self) -> Vec<Artifact> {
        Vec::new()
    }

    fn observe(&self) -> BoxFuture<'_, PageResult<Observation>> {
        let saved = *self.saved.lock().unwrap();
        let text = page_text(saved);
        Box::pin(async move {
            Ok(serde_json::from_value(json!({
                "url": "https://editor.test/", "title": "Editor", "page_key": if saved {"saved"} else {"editor"},
                "document_key": "editor", "captured_at": "2026-11-22T00:00:00Z", "tabs": [],
                "viewport_text": text,
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
        let text = page_text(*self.saved.lock().unwrap());
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

fn page_text(saved: bool) -> String {
    // A forum or settings page can hold thousands of unchanged lines beside its save status.
    let mut lines: Vec<_> = (0..2000).map(|at| format!("Unchanged page paragraph {at}.")).collect();
    lines.push(if saved { "Changes saved successfully" } else { "Not saved" }.into());
    lines.join("\n")
}

#[test]
fn saving_on_a_long_page_does_not_stall_the_chat_loop() {
    let (send, receive) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().start_paused(true).build().unwrap();
        let result = runtime.block_on(async {
            let page = Arc::new(SavePage { saved: Mutex::new(false) });
            let jev = Arc::new(SavingJev { script: ScriptedJev::new(steps("click Save; done").unwrap()) });
            let config: Config = serde_json::from_value(json!({"step_frames": false})).unwrap();
            let agent = Agent::new(page, jev, Arc::new(ScriptedDriver::new(steps("click Save; done").unwrap())))
                .with_config(config);
            agent
                .run(
                    "Save the description",
                    RunOptions {
                        start: Some("https://editor.test/".into()),
                        authorization: Some(Authorization { irreversible_actions: true }),
                        ..RunOptions::default()
                    },
                )
                .await
                .unwrap()
        });
        send.send(result).unwrap();
    });
    let _result = receive
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("saving beside 2,000 unchanged lines must finish within ten seconds without network or model calls");
}
