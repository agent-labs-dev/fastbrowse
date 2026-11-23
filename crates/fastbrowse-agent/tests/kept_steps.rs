//! Coverage sees saved changes after the loop has recorded their visible effect.

use std::sync::{Arc, Mutex};

use fastbrowse_agent::agent::{Agent, RunOptions};
use fastbrowse_agent::scripted::{ScriptedJev, ScriptedLLM, steps};
use fastbrowse_core::BoxFuture;
use fastbrowse_core::config::Config;
use fastbrowse_core::decision::{
    Answer, DecisionClient, DecisionError, DecisionProfile, Evaluation, NoulAnswer, NoulTag, Question,
};
use fastbrowse_core::models::{Artifact, Authorization, Limits, Operation, StepOutcome};
use fastbrowse_core::page::{ActResult, Action, BrowserError, Capture, Observation, Page, PageResult};
use indexmap::IndexMap;
use serde_json::{Value, json};

struct Editor(Mutex<bool>);

impl Page for Editor {
    fn artifacts(&self) -> Vec<Artifact> {
        Vec::new()
    }

    fn observe(&self) -> BoxFuture<'_, PageResult<Observation>> {
        let saved = *self.0.lock().unwrap();
        Box::pin(async move {
            Ok(serde_json::from_value(json!({
                "url": "https://editor.test/", "title": "Editor", "page_key": if saved { "saved" } else { "editor" },
                "document_key": "editor", "captured_at": "2026-11-18T00:00:00Z", "tabs": [],
                "viewport_text": if saved { "Saved." } else { "Editor" },
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
        Box::pin(async {
            Ok(serde_json::from_value(json!({
                "url": "https://editor.test/", "title": "Editor", "captured_at": "2026-11-18T00:00:00Z",
                "sha256": "", "text": "Saved.",
                "blocks": [{"source_id": "s0", "kind": "paragraph", "frame_id": null, "start": 0, "end": 6}]
            }))
            .unwrap())
        })
    }
    fn act<'a>(&'a self, action: &'a Action, _: &'a Observation) -> BoxFuture<'a, PageResult<ActResult>> {
        assert_eq!(action.operation, Operation::Click);
        assert_eq!(action.target_id.as_deref(), Some("save"));
        *self.0.lock().unwrap() = true;
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
        Box::pin(async { Err(BrowserError::ScreenshotsUnavailable("text-only test page".into())) })
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

struct Inspect {
    inner: ScriptedJev,
    coverage: Mutex<Vec<String>>,
    lasting: Mutex<bool>,
}

impl DecisionClient for Inspect {
    fn profile(&self) -> DecisionProfile {
        DecisionProfile::jev()
    }

    fn evaluate<'a>(
        &'a self,
        state: &'a Value,
        questions: &'a IndexMap<String, Question>,
    ) -> BoxFuture<'a, Result<Evaluation, DecisionError>> {
        Box::pin(async move {
            if let Some(Question::Noul(question)) = questions.get("coverage") {
                self.coverage.lock().unwrap().push(question.instructions.clone());
            }
            let mut reply = self.inner.evaluate(state, questions).await?;
            for key in ["requested", "lasting", "answers"] {
                if questions.contains_key(key) {
                    if key == "lasting" {
                        *self.lasting.lock().unwrap() = true;
                    }
                    reply.answers.insert(key.into(), Answer::Noul(NoulAnswer { kind: NoulTag, probability: 1.0 }));
                }
            }
            Ok(reply)
        })
    }
}

#[tokio::test]
async fn a_reversible_save_reaches_coverage_after_its_effect_is_recorded() {
    let jev = Arc::new(Inspect {
        inner: ScriptedJev::new(steps("click Save").unwrap()),
        coverage: Mutex::default(),
        lasting: Mutex::new(false),
    });
    let agent = Agent::new(Arc::new(Editor(Mutex::new(false))), jev.clone(), Arc::new(ScriptedLLM))
        .with_config(Config { plan_ahead: false, ..Config::default() });
    let result = agent
        .run(
            "Save the setting and report the status",
            RunOptions {
                start: Some("https://editor.test/".into()),
                authorization: Some(Authorization { irreversible_actions: true }),
                limits: Some(Limits { max_steps: Some(4), ..Limits::default() }),
                ..RunOptions::default()
            },
        )
        .await
        .unwrap();
    assert!(*jev.lasting.lock().unwrap(), "the gate must classify the save as lasting");
    assert_eq!(result.steps[0].outcome, StepOutcome::Executed);
    let coverage = jev.coverage.lock().unwrap();
    assert!(!coverage.is_empty(), "coverage must be checked: {result:?}");
    assert!(
        coverage.iter().all(|question| question.contains("The run carried out these steps:")
            && question.contains("\"target\": \"Save\"")),
        "coverage omitted the executed reversible save: {coverage:?}"
    );
}
