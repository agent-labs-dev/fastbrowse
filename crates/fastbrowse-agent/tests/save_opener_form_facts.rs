//! A gallery's edit control can belong to a form other than the editor it opens.

use std::sync::{Arc, Mutex};

use fastbrowse_agent::agent::{Agent, RunOptions};
use fastbrowse_agent::scripted::{ScriptedJev, ScriptedLLM, steps};
use fastbrowse_core::BoxFuture;
use fastbrowse_core::decision::{Answer, DecisionClient, DecisionError, DecisionProfile, Evaluation, Question};
use fastbrowse_core::llm::{Generation, LLMClient, LLMError, Message, ResponseSchema};
use fastbrowse_core::models::{
    Artifact, Authorization, CostBasis, CostComponent, CostLine, LLMPurpose, Limits, Operation, StepOutcome,
};
use fastbrowse_core::page::{ActResult, Action, BrowserError, Capture, Control, Observation, Page, PageResult};
use fastbrowse_core::telemetry::Ledger;
use indexmap::IndexMap;
use serde_json::{Value, json};

struct Gallery(Mutex<Observation>);

impl Page for Gallery {
    fn artifacts(&self) -> Vec<Artifact> {
        Vec::new()
    }
    fn observe(&self) -> BoxFuture<'_, PageResult<Observation>> {
        let page = self.0.lock().unwrap().clone();
        Box::pin(async move { Ok(page) })
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
                "url": "https://example.test/", "title": "Gallery", "captured_at": "2026-11-23T00:00:00Z",
                "sha256": "empty", "text": "", "blocks": [],
            }))
            .unwrap())
        })
    }
    fn act<'a>(&'a self, action: &'a Action, _: &'a Observation) -> BoxFuture<'a, PageResult<ActResult>> {
        let mut page = self.0.lock().unwrap();
        match action.operation {
            Operation::Click if action.target_id.as_deref() == Some("edit") => {
                page.controls.extend([
                    Control {
                        form_id: Some("editor-form".into()),
                        value: Some("Untitled".into()),
                        ..Control::new("title", "textbox", "Title", [Operation::Fill])
                    },
                    Control {
                        form_id: Some("editor-form".into()),
                        ..Control::new("apply", "button", "Apply", [Operation::Click])
                    },
                ]);
            }
            Operation::Fill => {
                page.controls.iter_mut().find(|c| c.id == "title").unwrap().value = action.text.clone();
            }
            Operation::Click if action.target_id.as_deref() == Some("apply") => {}
            other => panic!("unexpected action: {other:?}"),
        }
        page.page_key.push('x');
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
        Box::pin(async { Err(BrowserError::ScreenshotsUnavailable("scripted page".into())) })
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

struct Questions {
    script: ScriptedJev,
    saves: Arc<Mutex<Vec<String>>>,
}

impl DecisionClient for Questions {
    fn profile(&self) -> DecisionProfile {
        self.script.profile()
    }
    fn evaluate<'a>(
        &'a self,
        state: &'a Value,
        questions: &'a IndexMap<String, Question>,
    ) -> BoxFuture<'a, Result<Evaluation, DecisionError>> {
        Box::pin(async move {
            if let Some(Question::Noul(requested)) = questions.get("requested")
                && requested.instructions.contains("labelled 'Apply'")
            {
                self.saves.lock().unwrap().push(requested.instructions.clone());
            }
            let mut evaluation = self.script.evaluate(state, questions).await?;
            for (key, answer) in &mut evaluation.answers {
                if let Answer::Choice(answer) = answer
                    && key.ends_with(":source")
                {
                    answer.choice = "write".into();
                }
            }
            Ok(evaluation)
        })
    }
}

struct Typist;

impl LLMClient for Typist {
    fn generate_value<'a>(
        &'a self,
        purpose: LLMPurpose,
        messages: &'a [Message],
        schema: &'a dyn ResponseSchema,
        ledger: Option<&'a Ledger>,
    ) -> BoxFuture<'a, Result<Generation<Value>, LLMError>> {
        if purpose != LLMPurpose::Fill {
            return ScriptedLLM.generate_value(purpose, messages, schema, ledger);
        }
        Box::pin(async move {
            let data = schema.validate_json(r#"{"value":"New title"}"#).map_err(LLMError::schema)?;
            Ok(Generation { data, cost: CostLine::new(CostComponent::Llm, CostBasis::Estimated, Some(0.0)) })
        })
    }
}

#[tokio::test]
async fn a_save_names_an_opener_that_belongs_to_another_form() {
    // A bulk-selection form surrounds the cards, while the edit modal is a separate form.
    let edit = Control {
        form_id: Some("bulk-selection-form".into()),
        ..Control::new("edit", "button", "Edit second card", [Operation::Click])
    };
    let page = serde_json::from_value(json!({
        "url": "https://example.test/", "title": "Gallery", "page_key": "k", "document_key": "doc",
        "captured_at": "2026-11-23T00:00:00Z", "controls": [edit], "viewport_text": "Two untitled cards", "tabs": [],
    }))
    .unwrap();
    let saves = Arc::new(Mutex::new(Vec::new()));
    let agent = Agent::new(
        Arc::new(Gallery(Mutex::new(page))),
        Arc::new(Questions {
            script: ScriptedJev::new(steps("click Edit second card; fill Title; click Apply").unwrap()),
            saves: saves.clone(),
        }),
        Arc::new(Typist),
    );
    agent
        .run(
            "Retitle the first card to New title and leave the second card alone.",
            RunOptions {
                start: Some("https://example.test/".into()),
                authorization: Some(Authorization { irreversible_actions: true }),
                limits: Some(Limits { max_steps: Some(3), ..Limits::default() }),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let saves = saves.lock().unwrap();
    assert!(!saves.is_empty(), "the run must ask about the save");
    for question in &*saves {
        assert!(question.contains("which held 'Untitled', with 'New title'"), "{question}");
        assert!(question.contains("The run opened this form by clicking 'Edit second card'."), "{question}");
    }
}
